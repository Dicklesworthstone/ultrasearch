//! Metadata (filename/attributes) index built on Tantivy.
//!
//! This crate owns the schema for the "metadata" index described in the plan
//! (doc_key, volume, name, ext, size, timestamps, flags). For c00.3 we provide
//! a schema builder and a thin wrapper to open/create the index; the service
//! will wire the actual writer/reader later.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use core_types::{DocKey, FileMeta as CoreFileMeta};
use tantivy::{Index, IndexWriter, Term, schema::document::TantivyDocument, schema::*};

#[cfg(test)]
use tantivy::{IndexSettings, ReloadPolicy};

pub mod cache;
pub mod fst;
pub mod state;
pub mod tiers;

/// Fields used in the metadata index.
#[derive(Debug, Clone)]
pub struct MetaFields {
    pub doc_key: Field,
    pub volume: Field,
    pub name: Field,
    pub path: Field,
    pub ext: Field,
    pub size: Field,
    pub created: Field,
    pub modified: Field,
    pub flags: Field,
}

/// Build the Tantivy schema and return both `Schema` and typed field handles.
pub fn build_schema() -> (Schema, MetaFields) {
    let mut builder = Schema::builder();

    // Raw terms preserve the full volume + 64-bit FRN identity and make
    // delete-before-add replacement effective (FAST alone is not indexed).
    let doc_key = builder.add_text_field("doc_key", STRING | FAST | STORED);
    let volume = builder.add_u64_field("volume", INDEXED | FAST | STORED);
    let name = builder.add_text_field("name", TEXT | STORED);
    let path = builder.add_text_field("path", TEXT | STORED);
    let ext = builder.add_text_field("ext", STRING | FAST | STORED);
    let size = builder.add_u64_field("size", FAST | STORED);
    let created = builder.add_i64_field("created", FAST | STORED);
    let modified = builder.add_i64_field("modified", FAST | STORED);
    let flags = builder.add_u64_field("flags", FAST | STORED);

    let fields = MetaFields {
        doc_key,
        volume,
        name,
        path,
        ext,
        size,
        created,
        modified,
        flags,
    };

    (builder.build(), fields)
}

/// Lightweight document representation for ingest.
#[derive(Debug, Clone)]
pub struct MetaDoc {
    pub key: DocKey,
    pub volume: u16,
    pub name: String,
    pub path: Option<String>,
    pub ext: Option<String>,
    pub size: u64,
    pub created: i64,
    pub modified: i64,
    pub flags: u64,
}

impl From<&CoreFileMeta> for MetaDoc {
    fn from(f: &CoreFileMeta) -> Self {
        MetaDoc {
            key: f.key,
            volume: f.volume,
            name: f.name.clone(),
            path: f.path.clone(),
            ext: f.ext.clone(),
            size: f.size,
            created: f.created,
            modified: f.modified,
            flags: f.flags.bits() as u64,
        }
    }
}

/// Replace a batch of documents by their stable keys.
///
/// Delete and add operations are ordered by Tantivy, including repeated keys
/// within the same uncommitted batch. Replaying a committed batch is therefore
/// idempotent. The caller commits after applying all metadata mutations.
pub fn add_batch(
    writer: &mut IndexWriter,
    fields: &MetaFields,
    docs: impl IntoIterator<Item = MetaDoc>,
) -> Result<()> {
    for doc in docs {
        delete_doc(writer, fields, doc.key);
        writer.add_document(to_document(&doc, fields))?;
    }
    Ok(())
}

/// Queue deletion of every prior version of one document; commit separately.
pub fn delete_doc(writer: &mut IndexWriter, fields: &MetaFields, key: DocKey) {
    writer.delete_term(Term::from_field_text(fields.doc_key, &key.to_string()));
}

/// Queue deletion of a volume before a complete, gap-recovery enumeration.
/// Add replacement documents after this operation, then commit separately.
pub fn delete_volume(writer: &mut IndexWriter, fields: &MetaFields, volume: u16) {
    writer.delete_term(Term::from_field_u64(fields.volume, u64::from(volume)));
}

/// Add a batch of `core_types::FileMeta` records.
pub fn add_file_meta_batch(
    writer: &mut IndexWriter,
    fields: &MetaFields,
    metas: impl IntoIterator<Item = CoreFileMeta>,
) -> Result<()> {
    add_batch(writer, fields, metas.into_iter().map(|m| MetaDoc::from(&m)))
}

/// Convenience handle bundling an index with its field set.
#[derive(Debug)]
pub struct MetaIndex {
    pub index: Index,
    pub fields: MetaFields,
}

/// Open an existing index if it exists; otherwise create a fresh one.
///
/// This keeps the caller’s path semantics simple and mirror Tantivy’s typical
/// “open or create” ergonomics without forcing the caller to probe the
/// directory manually.
pub fn open_or_create_index(path: &Path) -> Result<MetaIndex> {
    let (schema, fields) = build_schema();
    let index = if path.join("meta.json").exists() {
        let index = Index::open_in_dir(path)?;
        validate_schema(&index).with_context(|| format!("metadata index {}", path.display()))?;
        index
    } else {
        std::fs::create_dir_all(path)?;
        Index::create_in_dir(path, schema)?
    };
    Ok(MetaIndex { index, fields })
}

/// Refuse legacy or incompatible field layouts before constructing handles.
///
/// The old numeric key discarded the NTFS sequence number and was not indexed,
/// so it cannot be migrated losslessly. The service must rebuild from NTFS and
/// reset its journal checkpoint instead of treating this as a usable index.
pub fn validate_schema(index: &Index) -> Result<()> {
    ensure!(
        index.schema() == build_schema().0,
        "incompatible metadata index schema; rebuild required for lossless document keys"
    );
    Ok(())
}

/// Writer configuration used during initial builds and batch updates.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Target heap size in bytes (e.g., 512 MiB for initial builds, smaller in service).
    pub heap_size_bytes: usize,
    /// Number of indexing threads; typically <= num_cpus.
    pub num_threads: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            heap_size_bytes: 512 * 1024 * 1024, // 512 MiB for initial metadata build
            num_threads: 4,
        }
    }
}

/// Create an `IndexWriter` with the provided configuration.
pub fn create_writer(meta: &MetaIndex, cfg: &WriterConfig) -> Result<IndexWriter> {
    meta.index
        .writer_with_num_threads(cfg.num_threads, cfg.heap_size_bytes)
        .map_err(Into::into)
}

/// Open a read-only handle with minimal caching suitable for the long-lived service.
pub fn open_reader(meta: &MetaIndex) -> Result<tantivy::IndexReader> {
    let reader = meta.index.reader_builder().try_into()?;
    Ok(reader)
}

/// Convert a `MetaDoc` into a Tantivy `Document`.
pub fn to_document(doc: &MetaDoc, fields: &MetaFields) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(fields.doc_key, doc.key.to_string());
    d.add_u64(fields.volume, doc.volume as u64);
    d.add_text(fields.name, &doc.name);
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
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::directory::RamDirectory;

    #[test]
    fn to_document_sets_fields() {
        let (_schema, fields) = build_schema();
        let doc = MetaDoc {
            key: DocKey::from_parts(9, 42),
            volume: 9,
            name: "sample.txt".into(),
            path: Some(r"C:\sample.txt".into()),
            ext: Some("txt".into()),
            size: 1234,
            created: 100,
            modified: 200,
            flags: 0b1010,
        };

        let tdoc = to_document(&doc, &fields);
        let get = |field| tdoc.get_first(field).unwrap();
        assert_eq!(get(fields.doc_key).as_str().unwrap(), doc.key.to_string());
        assert_eq!(get(fields.volume).as_u64().unwrap(), doc.volume as u64);
        assert_eq!(get(fields.size).as_u64().unwrap(), doc.size);
        assert_eq!(get(fields.created).as_i64().unwrap(), doc.created);
        assert_eq!(get(fields.modified).as_i64().unwrap(), doc.modified);
        assert_eq!(get(fields.flags).as_u64().unwrap(), doc.flags);
    }

    #[test]
    fn add_and_read_round_trip() -> Result<()> {
        let dir = RamDirectory::create();
        let (schema, fields) = build_schema();
        let index = Index::create(dir, schema, IndexSettings::default())?;
        let mut writer = index.writer_with_num_threads(1, 50_000_000)?;

        let docs = vec![
            MetaDoc {
                key: DocKey::from_parts(1, 10),
                volume: 1,
                name: "foo.txt".into(),
                path: Some("C:\\foo.txt".into()),
                ext: Some("txt".into()),
                size: 123,
                created: 1_700_000_000,
                modified: 1_700_000_100,
                flags: 0,
            },
            MetaDoc {
                key: DocKey::from_parts(2, 20),
                volume: 2,
                name: "bar.md".into(),
                path: Some("C:\\bar.md".into()),
                ext: Some("md".into()),
                size: 456,
                created: 1_700_000_200,
                modified: 1_700_000_300,
                flags: 0,
            },
        ];

        add_batch(&mut writer, &fields, docs.clone())?;
        writer.commit()?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        let searcher = reader.searcher();

        let all = tantivy::query::AllQuery;
        let top_docs = searcher.search(&all, &tantivy::collector::TopDocs::with_limit(10))?;
        assert_eq!(top_docs.len(), 2);

        let doc: TantivyDocument = searcher.doc(top_docs[0].1)?;
        let doc_key: DocKey = doc
            .get_first(fields.doc_key)
            .unwrap()
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(doc_key == docs[0].key || doc_key == docs[1].key);
        Ok(())
    }

    fn sample_meta(key: DocKey, name: &str, size: u64) -> MetaDoc {
        MetaDoc {
            key,
            volume: key.volume(),
            name: name.into(),
            path: Some(format!(r"C:\{name}")),
            ext: Some("txt".into()),
            size,
            created: 1,
            modified: size as i64,
            flags: 0,
        }
    }

    fn matches(index: &Index, field: Field, text: &str) -> Result<u64> {
        let query = tantivy::query::TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::Basic,
        );
        let reader = index.reader()?;
        Ok(reader
            .searcher()
            .search(&query, &tantivy::collector::Count)? as u64)
    }

    #[test]
    fn mutations_replace_replay_rename_and_delete_without_stale_hits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create_index(dir.path())?;
        let cfg = WriterConfig {
            heap_size_bytes: 20_000_000,
            num_threads: 1,
        };
        let mut writer = create_writer(&index, &cfg)?;
        let key = DocKey::from_parts(1, 0x0001_0000_0000_002a);
        let original = sample_meta(key, "original.txt", 10);
        // Replay within a batch must not append duplicate documents.
        add_batch(
            &mut writer,
            &index.fields,
            [original.clone(), original.clone()],
        )?;
        writer.commit()?;
        assert_eq!(matches(&index.index, index.fields.name, "original")?, 1);

        // A modification and an attribute update replace all stored metadata.
        let mut modified = original;
        modified.size = 42;
        modified.modified = 77;
        modified.flags = core_types::FileFlags::HIDDEN.bits() as u64;
        add_batch(&mut writer, &index.fields, [modified.clone()])?;
        writer.commit()?;
        let reader = index.index.reader()?;
        let docs = reader.searcher().search(
            &tantivy::query::AllQuery,
            &tantivy::collector::TopDocs::with_limit(10),
        )?;
        assert_eq!(docs.len(), 1);
        let stored: TantivyDocument = reader.searcher().doc(docs[0].1)?;
        assert_eq!(
            stored.get_first(index.fields.size).unwrap().as_u64(),
            Some(42)
        );
        assert_eq!(
            stored.get_first(index.fields.modified).unwrap().as_i64(),
            Some(77)
        );
        assert_eq!(
            stored.get_first(index.fields.flags).unwrap().as_u64(),
            Some(modified.flags)
        );

        let renamed = sample_meta(key, "renamed.txt", 42);
        add_batch(&mut writer, &index.fields, [renamed.clone()])?;
        writer.commit()?;
        assert_eq!(matches(&index.index, index.fields.name, "original")?, 0);
        assert_eq!(matches(&index.index, index.fields.name, "renamed")?, 1);
        drop(writer);
        drop(index);

        // Reopen and replay the last acknowledged batch after a restart.
        let reopened = open_or_create_index(dir.path())?;
        let mut writer = create_writer(&reopened, &cfg)?;
        add_batch(&mut writer, &reopened.fields, [renamed])?;
        writer.commit()?;
        assert_eq!(
            matches(&reopened.index, reopened.fields.name, "renamed")?,
            1
        );
        delete_doc(&mut writer, &reopened.fields, key);
        delete_doc(&mut writer, &reopened.fields, key);
        writer.commit()?;
        assert_eq!(reopened.index.reader()?.searcher().num_docs(), 0);
        Ok(())
    }

    #[test]
    fn full_frn_and_volume_resets_do_not_collide() -> Result<()> {
        let dir = RamDirectory::create();
        let (schema, fields) = build_schema();
        let index = Index::create(dir, schema, IndexSettings::default())?;
        let mut writer = index.writer_with_num_threads(1, 20_000_000)?;
        let old = DocKey::from_parts(1, 0x0001_0000_0000_002a);
        let reused = DocKey::from_parts(1, 0x0002_0000_0000_002a);
        let other_volume = DocKey::from_parts(2, old.file_id());
        add_batch(
            &mut writer,
            &fields,
            [
                sample_meta(old, "old.txt", 1),
                sample_meta(reused, "reused.txt", 2),
                sample_meta(other_volume, "other.txt", 3),
            ],
        )?;
        writer.commit()?;
        assert_eq!(index.reader()?.searcher().num_docs(), 3);
        delete_doc(&mut writer, &fields, old);
        writer.commit()?;
        assert_eq!(matches(&index, fields.name, "old")?, 0);
        assert_eq!(matches(&index, fields.name, "reused")?, 1);
        delete_volume(&mut writer, &fields, 1);
        add_batch(&mut writer, &fields, [sample_meta(reused, "fresh.txt", 4)])?;
        writer.commit()?;
        assert_eq!(index.reader()?.searcher().num_docs(), 2);
        assert_eq!(matches(&index, fields.name, "reused")?, 0);
        assert_eq!(matches(&index, fields.name, "fresh")?, 1);
        assert_eq!(matches(&index, fields.name, "other")?, 1);
        Ok(())
    }

    #[test]
    fn legacy_schema_requires_rebuild_without_overwriting_index() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut schema = Schema::builder();
        schema.add_u64_field("doc_key", FAST | STORED);
        let legacy = schema.build();
        let index = Index::create_in_dir(dir.path(), legacy.clone())?;
        drop(index);
        let before = std::fs::read(dir.path().join("meta.json"))?;
        let error = open_or_create_index(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("rebuild required"));
        assert_eq!(std::fs::read(dir.path().join("meta.json"))?, before);
        assert_eq!(Index::open_in_dir(dir.path())?.schema(), legacy);
        Ok(())
    }
}
