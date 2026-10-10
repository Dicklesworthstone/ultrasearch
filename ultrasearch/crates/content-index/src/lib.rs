//! Tantivy-based content index (full-text).
//!
//! Schema matches the plan: doc_key, volume, name/path/ext metadata, size,
//! modified, optional content_lang, and the main `content` text field.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use core_types::{DocKey, FileFlags, FileMeta};
pub use tantivy::IndexWriter;
use tantivy::{
    Index, IndexSettings, ReloadPolicy, Term, schema::document::TantivyDocument, schema::*,
};

pub mod log_analysis;

/// Field handles for the content index schema.
#[derive(Debug, Clone)]
pub struct ContentFields {
    pub doc_key: Field,
    pub volume: Field,
    pub name: Field,
    pub path: Field,
    pub ext: Field,
    pub size: Field,
    pub created: Field,
    pub modified: Field,
    pub flags: Field,
    pub content_lang: Field,
    pub content: Field,
}

pub fn build_schema() -> (Schema, ContentFields) {
    let mut builder = Schema::builder();

    let doc_key = builder.add_text_field("doc_key", STRING | FAST | STORED);
    let volume = builder.add_u64_field("volume", INDEXED | FAST | STORED);
    let name = builder.add_text_field("name", TEXT | STORED);
    let path = builder.add_text_field("path", TEXT | STORED);
    let ext = builder.add_text_field("ext", STRING | FAST | STORED);
    let size = builder.add_u64_field("size", FAST | STORED);
    let created = builder.add_i64_field("created", FAST | STORED);
    let modified = builder.add_i64_field("modified", FAST | STORED);
    let flags = builder.add_u64_field("flags", FAST | STORED);
    let content_lang = builder.add_text_field("content_lang", STRING | STORED);

    // Use default tokenizer for content, but allow overrides via per-field options later if needed.
    let content = builder.add_text_field("content", TEXT);

    let fields = ContentFields {
        doc_key,
        volume,
        name,
        path,
        ext,
        size,
        created,
        modified,
        flags,
        content_lang,
        content,
    };

    (builder.build(), fields)
}

#[derive(Debug)]
pub struct ContentIndex {
    pub index: Index,
    pub fields: ContentFields,
}

fn setup_index(index: &Index) {
    log_analysis::register_log_analyzers(index.tokenizers());
}

pub fn open_or_create(path: &Path) -> Result<ContentIndex> {
    let (schema, fields) = build_schema();
    let index = if path.join("meta.json").exists() {
        let index = Index::open_in_dir(path)?;
        validate_schema(&index).with_context(|| format!("content index {}", path.display()))?;
        index
    } else {
        std::fs::create_dir_all(path)?;
        Index::create_in_dir(path, schema)?
    };
    setup_index(&index);
    Ok(ContentIndex { index, fields })
}

/// Reject schemas whose identities cannot support lossless replacement.
/// Legacy numeric keys lost the NTFS sequence number; the service must rebuild
/// these indexes together with its metadata index and journal checkpoints.
pub fn validate_schema(index: &Index) -> Result<()> {
    ensure!(
        index.schema() == build_schema().0,
        "incompatible content index schema; rebuild required for lossless document keys"
    );
    Ok(())
}

/// Create an in-memory index for tests and benchmarks.
pub fn create_in_ram() -> Result<ContentIndex> {
    let (schema, fields) = build_schema();
    let dir = tantivy::directory::RamDirectory::create();
    let index = Index::create(dir, schema, IndexSettings::default())?;
    setup_index(&index);
    Ok(ContentIndex { index, fields })
}

#[derive(Debug, Clone)]
pub struct WriterConfig {
    pub heap_size_bytes: usize,
    pub num_threads: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            heap_size_bytes: 256 * 1024 * 1024, // conservative; content writer often heavier
            num_threads: 4,
        }
    }
}

pub fn create_writer(idx: &ContentIndex, cfg: &WriterConfig) -> Result<IndexWriter> {
    let writer = idx
        .index
        .writer_with_num_threads(cfg.num_threads, cfg.heap_size_bytes)?;
    Ok(writer)
}

pub fn open_reader(idx: &ContentIndex) -> Result<tantivy::IndexReader> {
    let reader = idx
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    Ok(reader)
}

#[derive(Debug, Clone)]
pub struct ContentDoc {
    pub key: DocKey,
    pub volume: u16,
    pub name: Option<String>,
    pub path: Option<String>,
    pub ext: Option<String>,
    pub size: u64,
    pub created: i64,
    pub modified: i64,
    pub flags: u64,
    pub content_lang: Option<String>,
    pub content: String,
}

pub fn to_document(doc: &ContentDoc, fields: &ContentFields) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(fields.doc_key, doc.key.to_string());
    d.add_u64(fields.volume, doc.volume as u64);
    if let Some(name) = &doc.name {
        d.add_text(fields.name, name);
    }
    if let Some(path) = &doc.path {
        d.add_text(fields.path, path);
    }
    if let Some(ext) = &doc.ext {
        d.add_text(fields.ext, ext);
    }
    d.add_u64(fields.size, doc.size);
    d.add_i64(fields.created, doc.created);
    d.add_i64(fields.modified, doc.modified);
    d.add_u64(fields.flags, doc.flags);
    if let Some(lang) = &doc.content_lang {
        d.add_text(fields.content_lang, lang);
    }
    d.add_text(fields.content, &doc.content);
    d
}

/// Replace the content and metadata of one document, idempotently on replay.
/// Delete-before-add also removes old full-text terms and paths on rename.
pub fn add_content_doc(
    writer: &mut IndexWriter,
    fields: &ContentFields,
    doc: &ContentDoc,
) -> Result<()> {
    let tdoc = to_document(doc, fields);
    delete_doc(writer, fields, doc.key);
    writer.add_document(tdoc)?;
    Ok(())
}

/// Queue deletion of all content versions of one document; commit separately.
pub fn delete_doc(writer: &mut IndexWriter, fields: &ContentFields, key: DocKey) {
    writer.delete_term(Term::from_field_text(fields.doc_key, &key.to_string()));
}

/// Queue a volume reset before rebuilding content after a journal gap.
pub fn delete_volume(writer: &mut IndexWriter, fields: &ContentFields, volume: u16) {
    writer.delete_term(Term::from_field_u64(fields.volume, u64::from(volume)));
}

/// Read the metadata of the exact filesystem snapshot a worker committed.
///
/// The caller opens/reloads the reader after worker acknowledgement and uses
/// these fields for the metadata-index commit. A missing document means the
/// reconciliation worker removed an obsolete path. Parent links are not needed
/// by the content index; callers may preserve them from the journal observation.
pub fn read_file_meta(
    index: &ContentIndex,
    reader: &tantivy::IndexReader,
    key: DocKey,
) -> Result<Option<FileMeta>> {
    let searcher = reader.searcher();
    let fields = &index.fields;
    let query = tantivy::query::TermQuery::new(
        Term::from_field_text(fields.doc_key, &key.to_string()),
        IndexRecordOption::Basic,
    );
    let hits = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(2))?;
    ensure!(
        hits.len() <= 1,
        "duplicate committed content identity {key}"
    );
    let Some((_, address)) = hits.first() else {
        return Ok(None);
    };
    let document: TantivyDocument = searcher.doc(*address)?;
    let text = |field| {
        document
            .get_first(field)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let unsigned = |field| document.get_first(field).and_then(|value| value.as_u64());
    let signed = |field| document.get_first(field).and_then(|value| value.as_i64());
    let volume = u16::try_from(unsigned(fields.volume).context("content volume missing")?)?;
    ensure!(
        volume == key.volume(),
        "content volume/key mismatch for {key}"
    );
    let flag_bits = u32::try_from(unsigned(fields.flags).context("content flags missing")?)?;
    let flags = FileFlags::from_bits(flag_bits).context("invalid content flags")?;
    Ok(Some(FileMeta {
        key,
        volume,
        parent: None,
        name: text(fields.name).context("content name missing")?,
        path: Some(text(fields.path).context("content path missing")?),
        ext: text(fields.ext),
        size: unsigned(fields.size).context("content size missing")?,
        created: signed(fields.created).context("content creation time missing")?,
        modified: signed(fields.modified).context("content modification time missing")?,
        flags,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::OwnedValue;

    #[test]
    fn schema_has_expected_fields() {
        let (schema, fields) = build_schema();
        for f in [
            fields.doc_key,
            fields.volume,
            fields.name,
            fields.path,
            fields.ext,
            fields.size,
            fields.created,
            fields.modified,
            fields.flags,
            fields.content_lang,
            fields.content,
        ] {
            assert!(!schema.get_field_entry(f).name().is_empty());
        }
    }

    #[test]
    fn to_document_sets_key_and_content() {
        let (_, fields) = build_schema();
        let doc = ContentDoc {
            key: DocKey::from_parts(1, 2),
            volume: 1,
            name: Some("file.txt".into()),
            path: Some(r"C:\file.txt".into()),
            ext: Some("txt".into()),
            size: 10,
            created: 100,
            modified: 123,
            flags: 0,
            content_lang: Some("en".into()),
            content: "hello world".into(),
        };
        let tantivy_doc = to_document(&doc, &fields);
        let mut vals = tantivy_doc.get_all(fields.doc_key);
        let first = vals.next().expect("doc_key set");
        let owned: OwnedValue = first.into();
        assert!(matches!(owned, OwnedValue::Str(v) if v == doc.key.to_string()));
        assert!(vals.next().is_none());
    }

    #[test]
    fn create_ram_index_works() {
        let idx = create_in_ram().unwrap();
        let reader = open_reader(&idx).unwrap();
        assert_eq!(reader.searcher().num_docs(), 0);
    }

    fn sample_doc(key: DocKey, name: &str, content: &str) -> ContentDoc {
        ContentDoc {
            key,
            volume: key.volume(),
            name: Some(name.into()),
            path: Some(format!(r"C:\{name}")),
            ext: Some("txt".into()),
            size: content.len() as u64,
            created: 100,
            modified: 123,
            flags: FileFlags::ARCHIVE.bits() as u64,
            content_lang: None,
            content: content.into(),
        }
    }

    fn matches(index: &ContentIndex, field: Field, text: &str) -> Result<usize> {
        let reader = open_reader(index)?;
        let query = tantivy::query::TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::Basic,
        );
        Ok(reader
            .searcher()
            .search(&query, &tantivy::collector::Count)?)
    }

    #[test]
    fn replay_modify_rename_and_delete_replace_stale_content() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create(dir.path())?;
        let cfg = WriterConfig {
            heap_size_bytes: 20_000_000,
            num_threads: 1,
        };
        let mut writer = create_writer(&index, &cfg)?;
        let key = DocKey::from_parts(7, 0xfedc_0000_0000_0042);
        let original = sample_doc(key, "original.txt", "obsoleteword");
        add_content_doc(&mut writer, &index.fields, &original)?;
        add_content_doc(&mut writer, &index.fields, &original)?;
        writer.commit()?;
        assert_eq!(matches(&index, index.fields.content, "obsoleteword")?, 1);

        let modified = sample_doc(key, "renamed.txt", "replacementword");
        add_content_doc(&mut writer, &index.fields, &modified)?;
        writer.commit()?;
        assert_eq!(matches(&index, index.fields.content, "obsoleteword")?, 0);
        assert_eq!(matches(&index, index.fields.name, "original")?, 0);
        assert_eq!(matches(&index, index.fields.name, "renamed")?, 1);
        assert_eq!(matches(&index, index.fields.content, "replacementword")?, 1);
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 1);
        let committed = read_file_meta(&index, &open_reader(&index)?, key)?.unwrap();
        assert_eq!(committed.name, "renamed.txt");
        assert_eq!(committed.path.as_deref(), Some(r"C:\renamed.txt"));
        assert_eq!(committed.size, modified.size);
        assert_eq!(committed.created, modified.created);
        assert_eq!(committed.modified, modified.modified);
        assert_eq!(committed.flags, FileFlags::ARCHIVE);
        drop(writer);
        drop(index);

        let reopened = open_or_create(dir.path())?;
        let mut writer = create_writer(&reopened, &cfg)?;
        add_content_doc(&mut writer, &reopened.fields, &modified)?;
        writer.commit()?;
        assert_eq!(
            matches(&reopened, reopened.fields.content, "replacementword")?,
            1
        );
        delete_doc(&mut writer, &reopened.fields, key);
        delete_doc(&mut writer, &reopened.fields, key);
        writer.commit()?;
        assert_eq!(open_reader(&reopened)?.searcher().num_docs(), 0);
        assert!(read_file_meta(&reopened, &open_reader(&reopened)?, key)?.is_none());
        Ok(())
    }

    #[test]
    fn full_frn_identity_and_volume_reset_are_isolated() -> Result<()> {
        let index = create_in_ram()?;
        let cfg = WriterConfig {
            heap_size_bytes: 20_000_000,
            num_threads: 1,
        };
        let mut writer = create_writer(&index, &cfg)?;
        let old = DocKey::from_parts(1, 0x0001_0000_0000_002a);
        let reused = DocKey::from_parts(1, 0x0002_0000_0000_002a);
        let other = DocKey::from_parts(2, old.file_id());
        for doc in [
            sample_doc(old, "old.txt", "oldword"),
            sample_doc(reused, "reused.txt", "reusedword"),
            sample_doc(other, "other.txt", "otherword"),
        ] {
            add_content_doc(&mut writer, &index.fields, &doc)?;
        }
        writer.commit()?;
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 3);
        delete_doc(&mut writer, &index.fields, old);
        writer.commit()?;
        assert_eq!(matches(&index, index.fields.content, "oldword")?, 0);
        assert_eq!(matches(&index, index.fields.content, "reusedword")?, 1);
        delete_volume(&mut writer, &index.fields, 1);
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(reused, "fresh.txt", "freshword"),
        )?;
        writer.commit()?;
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 2);
        assert_eq!(matches(&index, index.fields.content, "reusedword")?, 0);
        assert_eq!(matches(&index, index.fields.content, "freshword")?, 1);
        assert_eq!(matches(&index, index.fields.content, "otherword")?, 1);
        Ok(())
    }

    #[test]
    fn incompatible_schema_requires_rebuild_and_preserves_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut schema = Schema::builder();
        schema.add_u64_field("doc_key", FAST | STORED);
        let legacy = schema.build();
        drop(Index::create_in_dir(dir.path(), legacy.clone())?);
        let before = std::fs::read(dir.path().join("meta.json"))?;
        let error = open_or_create(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("rebuild required"));
        assert_eq!(std::fs::read(dir.path().join("meta.json"))?, before);
        assert_eq!(Index::open_in_dir(dir.path())?.schema(), legacy);
        Ok(())
    }
}

#[test]
fn add_content_doc_appends() {
    let idx = create_in_ram().expect("in ram");
    let mut writer = create_writer(&idx, &WriterConfig::default()).unwrap();
    let doc = ContentDoc {
        key: DocKey::from_parts(1, 2),
        volume: 1,
        name: Some("foo.txt".into()),
        path: Some(r"C:\foo.txt".into()),
        ext: Some("txt".into()),
        size: 10,
        created: 100,
        modified: 123,
        flags: 0,
        content_lang: Some("en".into()),
        content: "hello world".into(),
    };
    add_content_doc(&mut writer, &idx.fields, &doc).unwrap();
    writer.commit().unwrap();
    let reader = open_reader(&idx).unwrap();
    let searcher = reader.searcher();
    assert_eq!(searcher.num_docs(), 1);
}
