//! Metadata (filename/attributes) index built on Tantivy.
//!
//! This crate owns the schema for the "metadata" index described in the plan
//! (doc_key, volume, name, ext, size, timestamps, flags). For c00.3 we provide
//! a schema builder and a thin wrapper to open/create the index; the service
//! will wire the actual writer/reader later.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use core_types::{
    DocKey, FileFlags, FileMeta as CoreFileMeta, VolumeId, index_path_matches, normalize_index_path,
};
use tantivy::query::{BooleanQuery, EmptyQuery, Occur, Query, RegexQuery, TermQuery};
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
    pub path_exact: Field,
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
    let path_exact = builder.add_text_field("path_exact", STRING);
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
        path_exact,
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
        "incompatible metadata index schema; rebuild required for lossless keys and normalized paths"
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

fn normalized_paths(paths: &[String]) -> Result<Vec<String>> {
    let mut normalized = paths
        .iter()
        .map(|path| normalize_index_path(path))
        .collect::<Vec<_>>();
    ensure!(
        normalized.iter().all(|path| !path.is_empty()),
        "directory path must not be empty"
    );
    normalized.sort_unstable();
    normalized.dedup();
    Ok(normalized)
}

/// Match literal normalized directory paths and their descendants. Regex
/// metacharacters in filenames are escaped; separator boundaries keep sibling
/// names out. This query is suitable for search visibility masks. Its Tantivy
/// scorer may materialize a segment bitset, so ingestion uses PathScan instead.
pub fn path_prefix_query(field: Field, paths: &[String]) -> Result<Box<dyn Query>> {
    let paths = normalized_paths(paths)?;
    if paths.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    let mut clauses = Vec::with_capacity(paths.len());
    for path in paths {
        let escaped = regex::escape(&path);
        // Tantivy's automaton matches the entire raw term. The optional suffix
        // is either absent or begins with a literal Windows separator. Include
        // newlines as well as ordinary characters rather than narrowing paths.
        let pattern = format!(r"{escaped}(\\(.|\n)*)?");
        clauses.push((
            Occur::Should,
            Box::new(RegexQuery::from_pattern(&pattern, field)?) as Box<dyn Query>,
        ));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

/// A frozen metadata snapshot with bounded document paging. Each turn examines
/// at most `limit` raw document/segment positions, counting deleted and unrelated
/// records toward the budget. Only matching live subtree metadata is returned;
/// no unrelated filesystem file needs to be opened or extracted.
pub struct PathScan {
    searcher: tantivy::Searcher,
    fields: MetaFields,
    volume: VolumeId,
    paths: Vec<String>,
    segment: usize,
    document: u32,
    failed: bool,
}

impl PathScan {
    pub fn new(index: &MetaIndex, volume: VolumeId, paths: &[String]) -> Result<Self> {
        ensure!(volume != 0, "path scan requires a bound volume identity");
        let paths = normalized_paths(paths)?;
        let searcher = open_reader(index)?.searcher();
        Ok(Self {
            searcher,
            fields: index.fields.clone(),
            volume,
            paths,
            segment: 0,
            document: 0,
            failed: false,
        })
    }

    /// Empty pages indicate bounded progress, including across tombstones and
    /// unrelated paths. Only None is EOF. The captured Searcher pins its segment
    /// files and deletion view across replacements, commits and index reloads.
    /// A failed page invalidates this cursor instead of silently skipping it.
    pub fn next_batch(&mut self, limit: usize) -> Result<Option<Vec<CoreFileMeta>>> {
        ensure!(limit > 0, "path scan limit must be nonzero");
        ensure!(!self.failed, "path scan failed; start a new frozen scan");
        let result = self.read_batch(limit);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn read_batch(&mut self, limit: usize) -> Result<Option<Vec<CoreFileMeta>>> {
        if self.paths.is_empty() || self.segment >= self.searcher.segment_readers().len() {
            return Ok(None);
        }
        let mut batch = Vec::with_capacity(limit.min(128));
        for _ in 0..limit {
            let Some(segment) = self.searcher.segment_readers().get(self.segment) else {
                break;
            };
            if self.document >= segment.max_doc() {
                self.segment += 1;
                self.document = 0;
                continue;
            }
            let document = self.document;
            self.document += 1;
            if segment.is_deleted(document) {
                continue;
            }
            let address = tantivy::DocAddress::new(self.segment as u32, document);
            let stored: TantivyDocument = self.searcher.doc(address)?;
            let volume = VolumeId::try_from(stored_unsigned(&stored, self.fields.volume)?)?;
            if volume != self.volume {
                continue;
            }
            let Some(path) = stored_text(&stored, self.fields.path)? else {
                continue;
            };
            if self
                .paths
                .iter()
                .any(|directory| index_path_matches(&path, directory))
            {
                batch.push(stored_file_meta(&stored, &self.fields)?);
            }
        }
        Ok(Some(batch))
    }
}

/// Look up one live metadata identity, rejecting duplicate or malformed stored
/// records instead of choosing an arbitrary pathname as a reconciliation root.
pub fn file_meta(index: &MetaIndex, key: DocKey) -> Result<Option<CoreFileMeta>> {
    let searcher = open_reader(index)?.searcher();
    let query = TermQuery::new(
        Term::from_field_text(index.fields.doc_key, &key.to_string()),
        IndexRecordOption::Basic,
    );
    let hits = searcher.search(&query, &tantivy::collector::TopDocs::with_limit(2))?;
    ensure!(
        hits.len() <= 1,
        "duplicate committed metadata identity {key}"
    );
    let Some((_, address)) = hits.first() else {
        return Ok(None);
    };
    let stored: TantivyDocument = searcher.doc(*address)?;
    let meta = stored_file_meta(&stored, &index.fields)?;
    ensure!(
        meta.key == key,
        "stored metadata identity does not match {key}"
    );
    Ok(Some(meta))
}

fn stored_text(document: &TantivyDocument, field: Field) -> Result<Option<String>> {
    let mut values = document.get_all(field);
    let text = values
        .next()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("invalid stored metadata text")
        })
        .transpose()?;
    ensure!(
        values.next().is_none(),
        "duplicate stored metadata field {field:?}"
    );
    Ok(text)
}

fn stored_unsigned(document: &TantivyDocument, field: Field) -> Result<u64> {
    let mut values = document.get_all(field);
    let number = values
        .next()
        .and_then(|value| value.as_u64())
        .context("missing or invalid stored unsigned metadata")?;
    ensure!(
        values.next().is_none(),
        "duplicate stored metadata field {field:?}"
    );
    Ok(number)
}

fn stored_signed(document: &TantivyDocument, field: Field) -> Result<i64> {
    let mut values = document.get_all(field);
    let number = values
        .next()
        .and_then(|value| value.as_i64())
        .context("missing or invalid stored signed metadata")?;
    ensure!(
        values.next().is_none(),
        "duplicate stored metadata field {field:?}"
    );
    Ok(number)
}

fn stored_file_meta(document: &TantivyDocument, fields: &MetaFields) -> Result<CoreFileMeta> {
    let key: DocKey = stored_text(document, fields.doc_key)?
        .context("metadata identity missing")?
        .parse()
        .map_err(anyhow::Error::msg)?;
    let volume = VolumeId::try_from(stored_unsigned(document, fields.volume)?)?;
    ensure!(
        volume == key.volume(),
        "metadata volume/key mismatch for {key}"
    );
    let flags = u32::try_from(stored_unsigned(document, fields.flags)?)?;
    Ok(CoreFileMeta {
        key,
        volume,
        parent: None,
        name: stored_text(document, fields.name)?.context("metadata name missing")?,
        path: stored_text(document, fields.path)?,
        ext: stored_text(document, fields.ext)?,
        size: stored_unsigned(document, fields.size)?,
        created: stored_signed(document, fields.created)?,
        modified: stored_signed(document, fields.modified)?,
        flags: FileFlags::from_bits(flags).context("invalid metadata flags")?,
    })
}

/// Convert a `MetaDoc` into a Tantivy `Document`.
pub fn to_document(doc: &MetaDoc, fields: &MetaFields) -> TantivyDocument {
    let mut d = TantivyDocument::default();
    d.add_text(fields.doc_key, doc.key.to_string());
    d.add_u64(fields.volume, doc.volume as u64);
    d.add_text(fields.name, &doc.name);
    if let Some(path) = &doc.path {
        d.add_text(fields.path, path);
        d.add_text(fields.path_exact, normalize_index_path(path));
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

    fn path_doc(volume: VolumeId, file: u64, path: &str) -> MetaDoc {
        let name = path
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or("root");
        let mut doc = sample_meta(DocKey::from_parts(volume, file), name, file);
        doc.path = Some(path.into());
        doc
    }

    #[test]
    fn path_prefix_queries_escape_names_normalize_paths_and_respect_boundaries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create_index(dir.path())?;
        let mut writer = index.index.writer_with_num_threads(1, 20_000_000)?;
        let root = r"\\?\Volume{ABCD}\Reports[2026](Final)+$^";
        let paths = [
            root.to_string(),
            format!(r"{root}\Sub\FILE.TXT"),
            "//?/VOLUME{abcd}/REPORTS[2026](final)+$^/Other.TXT".into(),
            format!("{root}\\line\nbreak.txt"),
            format!(r"{root}-old\sibling.txt"),
            r"\\?\Volume{ABCD}\Reports2Final\lookalike.txt".into(),
            r"\\?\Volume{FFFF}\Reports[2026](Final)+$^\other.txt".into(),
        ];
        add_batch(
            &mut writer,
            &index.fields,
            paths
                .iter()
                .enumerate()
                .map(|(i, path)| path_doc(1, i as u64 + 1, path)),
        )?;
        writer.commit()?;
        let query = path_prefix_query(
            index.fields.path_exact,
            &[format!("{root}\\"), normalize_index_path(root)],
        )?;
        let searcher = open_reader(&index)?.searcher();
        assert_eq!(searcher.search(&query, &tantivy::collector::Count)?, 4);
        let none = path_prefix_query(index.fields.path_exact, &[])?;
        assert_eq!(searcher.search(&none, &tantivy::collector::Count)?, 0);
        assert!(path_prefix_query(index.fields.path_exact, &["///".into()]).is_err());
        let stored = file_meta(&index, DocKey::from_parts(1, 3))?.unwrap();
        assert_eq!(stored.path.as_deref(), Some(paths[2].as_str()));
        assert_eq!(
            matches(
                &index.index,
                index.fields.path_exact,
                &normalize_index_path(&paths[2])
            )?,
            1
        );
        assert_eq!(matches(&index.index, index.fields.path_exact, "other")?, 0);
        Ok(())
    }

    #[test]
    fn path_scan_budgets_deleted_unrelated_and_cross_volume_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create_index(dir.path())?;
        let mut writer = index.index.writer_with_num_threads(1, 20_000_000)?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        add_batch(
            &mut writer,
            &index.fields,
            [
                path_doc(1, 1, r"C:\unrelated\first.txt"),
                path_doc(1, 2, r"C:\target\deleted.txt"),
                path_doc(1, 3, r"C:\TARGET\firstlive.txt"),
                path_doc(2, 4, r"C:\target\othervolume.txt"),
            ],
        )?;
        writer.commit()?;
        delete_doc(&mut writer, &index.fields, DocKey::from_parts(1, 2));
        let mut no_path = sample_meta(DocKey::from_parts(1, 7), "nopath.txt", 7);
        no_path.path = None;
        add_batch(
            &mut writer,
            &index.fields,
            [
                path_doc(1, 5, "C:/target/secondlive.txt"),
                path_doc(1, 6, r"C:\target-sibling\outside.txt"),
                no_path,
            ],
        )?;
        writer.commit()?;
        let mut scan = PathScan::new(&index, 1, &["c:/TARGET/".into()])?;
        assert!(scan.searcher.segment_readers().len() >= 2);
        let expected_turns = scan
            .searcher
            .segment_readers()
            .iter()
            .map(|segment| segment.max_doc() as usize + 1)
            .sum::<usize>();
        assert!(scan.next_batch(0).is_err());
        let mut turns = 0;
        let mut empty = 0;
        let mut found = std::collections::BTreeMap::new();
        while let Some(batch) = scan.next_batch(1)? {
            turns += 1;
            assert!(turns <= expected_turns);
            assert!(batch.len() <= 1);
            if batch.is_empty() {
                empty += 1;
            }
            for meta in batch {
                assert_eq!(meta.volume, 1);
                assert!(meta.parent.is_none());
                assert_eq!(Some(meta.clone()), file_meta(&index, meta.key)?);
                assert!(found.insert(meta.key, meta).is_none());
            }
        }
        assert_eq!(turns, expected_turns);
        assert_eq!(empty, expected_turns - 2);
        assert_eq!(
            found.keys().copied().collect::<Vec<_>>(),
            vec![DocKey::from_parts(1, 3), DocKey::from_parts(1, 5)]
        );
        assert!(scan.next_batch(1)?.is_none());
        assert!(PathScan::new(&index, 0, &["C:\\target".into()]).is_err());
        assert!(PathScan::new(&index, 1, &[])?.next_batch(1)?.is_none());
        Ok(())
    }

    #[test]
    fn path_scan_keeps_its_original_documents_across_replacements_and_commits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create_index(dir.path())?;
        let mut writer = index.index.writer_with_num_threads(1, 20_000_000)?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        for doc in [
            path_doc(1, 10, r"C:\source\first.txt"),
            path_doc(1, 20, r"C:\source\second.txt"),
        ] {
            add_batch(&mut writer, &index.fields, [doc])?;
            writer.commit()?;
        }
        let expected = [10, 20]
            .into_iter()
            .map(|id| {
                let key = DocKey::from_parts(1, id);
                Ok((
                    key,
                    file_meta(&index, key)?.context("initial file missing")?,
                ))
            })
            .collect::<Result<std::collections::BTreeMap<_, _>>>()?;
        let mut frozen = PathScan::new(&index, 1, &[r"C:\source".into()])?;
        let first = frozen.next_batch(1)?.context("first page missing")?;
        assert_eq!(first.len(), 1);
        let first_key = first[0].key;
        let unread = expected
            .keys()
            .copied()
            .find(|key| *key != first_key)
            .unwrap();
        add_batch(
            &mut writer,
            &index.fields,
            [
                path_doc(1, unread.file_id(), r"C:\elsewhere\moved.txt"),
                path_doc(1, 30, r"C:\source\arrived-later.txt"),
            ],
        )?;
        writer.commit()?;

        let mut original = first
            .into_iter()
            .map(|meta| (meta.key, meta))
            .collect::<std::collections::BTreeMap<_, _>>();
        while let Some(batch) = frozen.next_batch(1)? {
            assert!(batch.len() <= 1);
            for meta in batch {
                assert!(original.insert(meta.key, meta).is_none());
            }
        }
        assert_eq!(original, expected);
        let mut current = PathScan::new(&index, 1, &[r"C:\source".into()])?;
        let mut current_keys = std::collections::BTreeSet::new();
        while let Some(batch) = current.next_batch(2)? {
            assert!(batch.len() <= 2);
            current_keys.extend(batch.into_iter().map(|meta| meta.key));
        }
        assert_eq!(
            current_keys,
            std::collections::BTreeSet::from([first_key, DocKey::from_parts(1, 30)])
        );
        assert_eq!(
            file_meta(&index, unread)?.unwrap().path.as_deref(),
            Some(r"C:\elsewhere\moved.txt")
        );
        Ok(())
    }

    #[test]
    fn metadata_lookup_and_scan_reject_duplicate_or_invalid_records() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create_index(dir.path())?;
        let mut writer = index.index.writer_with_num_threads(1, 20_000_000)?;
        let key = DocKey::from_parts(1, 0x0003_0000_0000_0042);
        let original = path_doc(1, key.file_id(), r"C:\target\identity.txt");
        add_batch(&mut writer, &index.fields, [original.clone()])?;
        writer.commit()?;
        let found = file_meta(&index, key)?.unwrap();
        assert_eq!(found.key, key);
        assert_eq!(found.path, original.path);
        assert_eq!(found.name, original.name);
        assert_eq!(found.size, original.size);
        assert_eq!(found.created, original.created);
        assert_eq!(found.modified, original.modified);
        assert!(file_meta(&index, DocKey::from_parts(2, key.file_id()))?.is_none());
        writer.add_document(to_document(&original, &index.fields))?;
        writer.commit()?;
        assert!(format!("{:#}", file_meta(&index, key).unwrap_err()).contains("duplicate"));

        for corrupt in 0..3 {
            delete_doc(&mut writer, &index.fields, key);
            let mut invalid = original.clone();
            if corrupt == 0 {
                invalid.volume = 2;
            } else if corrupt == 1 {
                invalid.flags = u64::MAX;
            }
            let mut document = to_document(&invalid, &index.fields);
            if corrupt == 2 {
                document.add_u64(index.fields.size, 100);
            }
            writer.add_document(document)?;
            writer.commit()?;
            assert!(file_meta(&index, key).is_err());
        }
        let mut scan = PathScan::new(&index, 1, &[r"C:\target".into()])?;
        assert!(scan.next_batch(128).is_err());
        let error = scan.next_batch(128).unwrap_err();
        assert!(format!("{error:#}").contains("start a new frozen scan"));
        Ok(())
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
