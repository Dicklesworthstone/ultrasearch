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
use uuid::Uuid;

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

/// Evidence stored atomically with one physical index commit. Replaying a batch
/// generates a new commit identity, so restoring an older snapshot of that same
/// batch cannot masquerade as the completed checkpoint's exact index state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchReceipt {
    pub batch_id: Uuid,
    pub commit_id: Uuid,
    pub complete: bool,
}

/// A bounded set of file-level retries can complete with a batch. These keys
/// have no live content document and must become durable retry obligations in
/// the service's checkpoint before that checkpoint retires its pending intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome {
    pub receipt: BatchReceipt,
    pub deferred: Vec<DocKey>,
}

pub const MAX_DEFERRED_KEYS: usize = 1024;
// A canonical key uses at most 24 bytes, plus its comma separator. Bound the
// payload before parsing its keys, including invalid externally written input.
const MAX_BATCH_PAYLOAD_BYTES: usize = 128 + MAX_DEFERRED_KEYS * 25;

impl BatchOutcome {
    fn payload(&self) -> Result<String> {
        ensure!(
            !self.receipt.batch_id.is_nil() && !self.receipt.commit_id.is_nil(),
            "index batch receipt identities must not be nil"
        );
        validate_deferred(self.receipt.complete, &self.deferred)?;
        let mut payload = format!(
            "ultrasearch-ingestion-v2:{}:{}:{}:",
            self.receipt.batch_id,
            self.receipt.commit_id,
            if self.receipt.complete {
                "complete"
            } else {
                "partial"
            }
        );
        for (position, key) in self.deferred.iter().enumerate() {
            if position > 0 {
                payload.push(',');
            }
            payload.push_str(&key.to_string());
        }
        Ok(payload)
    }
}

fn validate_deferred(complete: bool, deferred: &[DocKey]) -> Result<()> {
    ensure!(
        deferred.len() <= MAX_DEFERRED_KEYS,
        "index batch exceeds the deferred key limit"
    );
    ensure!(
        complete || deferred.is_empty(),
        "partial receipts cannot publish deferred obligations"
    );
    ensure!(
        deferred.windows(2).all(|keys| keys[0] < keys[1]),
        "deferred keys must be sorted and unique"
    );
    ensure!(
        deferred
            .iter()
            .all(|key| *key == DocKey::from_parts(key.volume(), key.file_id())),
        "invalid deferred document identity"
    );
    Ok(())
}

/// Publish the successful batch identity atomically with its index mutations,
/// including an empty batch. The service persists both indexes' exact physical
/// commit identities before retiring its durable pending intent.
pub fn commit_batch(writer: &mut IndexWriter, id: Uuid) -> Result<tantivy::Opstamp> {
    commit_batch_with_deferred(writer, id, &[])
}

/// Atomically publish a completed batch and its remaining per-file retry
/// obligations. `deferred` must contain canonical keys in strictly increasing
/// order, with no duplicates. A completed receipt attests that these omissions
/// were recorded, not that every extraction succeeded.
pub fn commit_batch_with_deferred(
    writer: &mut IndexWriter,
    id: Uuid,
    deferred: &[DocKey],
) -> Result<tantivy::Opstamp> {
    commit_receipt(writer, id, true, deferred)
}

/// Publish a subset of a batch without attesting to completion. This includes
/// periodic commits and any commit made by a worker that encountered a failed
/// job. Only a matching durable pending intent permits recovery of this state.
pub fn commit_partial_batch(writer: &mut IndexWriter, id: Uuid) -> Result<tantivy::Opstamp> {
    commit_receipt(writer, id, false, &[])
}

fn commit_receipt(
    writer: &mut IndexWriter,
    id: Uuid,
    complete: bool,
    deferred: &[DocKey],
) -> Result<tantivy::Opstamp> {
    ensure!(!id.is_nil(), "index batch identity must not be nil");
    validate_deferred(complete, deferred)?;
    let outcome = BatchOutcome {
        receipt: BatchReceipt {
            batch_id: id,
            commit_id: Uuid::new_v4(),
            complete,
        },
        deferred: deferred.to_vec(),
    };
    let payload = outcome.payload()?;
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&payload);
    Ok(prepared.commit()?)
}

/// Read a receipt from the committed index metadata, not a sidecar or reader
/// cache. Missing receipts identify legacy or externally modified indexes;
/// invalid receipts require recovery rather than assuming journal coverage.
/// A partial receipt must never acknowledge work or validate a finished cursor.
pub fn batch_receipt(index: &Index) -> Result<Option<BatchReceipt>> {
    Ok(batch_outcome(index)?.map(|outcome| outcome.receipt))
}

/// Read both physical commit evidence and the file-level retry obligations
/// committed with it. Legacy v1 receipts have no deferred keys. All consumers
/// share this strict parser so malformed outcomes cannot be accepted as ACKs.
pub fn batch_outcome(index: &Index) -> Result<Option<BatchOutcome>> {
    index
        .load_metas()?
        .payload
        .map(|payload| {
            ensure!(
                payload.len() <= MAX_BATCH_PAYLOAD_BYTES,
                "index batch receipt payload exceeds its byte limit"
            );
            let mut parts = payload.splitn(5, ':');
            let version = match parts.next() {
                Some("ultrasearch-ingestion-v1") => 1,
                Some("ultrasearch-ingestion-v2") => 2,
                _ => anyhow::bail!("unsupported index batch receipt format"),
            };
            let batch_id = Uuid::parse_str(parts.next().context("missing receipt batch id")?)
                .context("invalid receipt batch id")?;
            let commit_id = Uuid::parse_str(parts.next().context("missing receipt commit id")?)
                .context("invalid receipt commit id")?;
            ensure!(
                !batch_id.is_nil() && !commit_id.is_nil(),
                "index batch receipt identities must not be nil"
            );
            let complete = match parts.next() {
                Some("complete") => true,
                Some("partial") => false,
                _ => anyhow::bail!("invalid receipt completion state"),
            };
            let mut deferred = Vec::new();
            if version == 1 {
                ensure!(parts.next().is_none(), "unexpected index receipt fields");
            } else {
                let encoded = parts.next().context("missing deferred key list")?;
                if !encoded.is_empty() {
                    for encoded_key in encoded.split(',') {
                        ensure!(
                            deferred.len() < MAX_DEFERRED_KEYS,
                            "index batch exceeds the deferred key limit"
                        );
                        let key = encoded_key.parse::<DocKey>().map_err(anyhow::Error::msg)?;
                        ensure!(
                            key.to_string() == encoded_key,
                            "deferred document identity is not canonical"
                        );
                        deferred.push(key);
                    }
                }
            }
            validate_deferred(complete, &deferred)?;
            Ok(BatchOutcome {
                receipt: BatchReceipt {
                    batch_id,
                    commit_id,
                    complete,
                },
                deferred,
            })
        })
        .transpose()
}

/// Return the batch identity only for a completed physical commit. Startup must
/// additionally compare the full receipt with its saved checkpoint, because a
/// previous partial or complete attempt can share the same replayable batch ID.
pub fn committed_batch(index: &Index) -> Result<Option<Uuid>> {
    Ok(batch_receipt(index)?
        .filter(|receipt| receipt.complete)
        .map(|receipt| receipt.batch_id))
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
    fn batch_receipt_persists_with_replacement_and_empty_commits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create(dir.path())?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        assert_eq!(committed_batch(&index.index)?, None);
        let seed = Uuid::new_v4();
        commit_batch(&mut writer, seed)?;
        assert_eq!(committed_batch(&index.index)?, Some(seed));
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 0);

        let key = DocKey::from_parts(7, 0xfedc_0000_0000_0042);
        let created = Uuid::new_v4();
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "original.txt", "obsoleteword"),
        )?;
        commit_batch(&mut writer, created)?;
        let replaced = Uuid::new_v4();
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "renamed.txt", "replacementword"),
        )?;
        commit_batch(&mut writer, replaced)?;
        let receipt = batch_receipt(&index.index)?;
        drop(writer);
        drop(index);

        let index = open_or_create(dir.path())?;
        assert_eq!(committed_batch(&index.index)?, Some(replaced));
        assert_eq!(batch_receipt(&index.index)?, receipt);
        assert_eq!(matches(&index, index.fields.content, "obsoleteword")?, 0);
        assert_eq!(matches(&index, index.fields.content, "replacementword")?, 1);
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let deleted = Uuid::new_v4();
        delete_doc(&mut writer, &index.fields, key);
        commit_batch(&mut writer, deleted)?;
        assert_eq!(committed_batch(&index.index)?, Some(deleted));
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 0);
        let reset = Uuid::new_v4();
        delete_volume(&mut writer, &index.fields, 7);
        commit_batch(&mut writer, reset)?;
        assert_eq!(committed_batch(&index.index)?, Some(reset));
        Ok(())
    }

    #[test]
    fn aborted_batch_preserves_committed_receipt_and_documents() -> Result<()> {
        let index = create_in_ram()?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let key = DocKey::from_parts(7, 42);
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "kept.txt", "committedword"),
        )?;
        let committed = Uuid::new_v4();
        commit_batch(&mut writer, committed)?;
        let receipt = batch_receipt(&index.index)?;
        delete_doc(&mut writer, &index.fields, key);
        let mut prepared = writer.prepare_commit()?;
        prepared.set_payload(
            &BatchOutcome {
                receipt: BatchReceipt {
                    batch_id: Uuid::new_v4(),
                    commit_id: Uuid::new_v4(),
                    complete: true,
                },
                deferred: vec![key],
            }
            .payload()?,
        );
        assert_eq!(committed_batch(&index.index)?, Some(committed));
        assert_eq!(batch_receipt(&index.index)?, receipt);
        assert_eq!(matches(&index, index.fields.content, "committedword")?, 1);
        prepared.abort()?;
        assert_eq!(committed_batch(&index.index)?, Some(committed));
        assert_eq!(batch_receipt(&index.index)?, receipt);
        assert_eq!(matches(&index, index.fields.content, "committedword")?, 1);
        Ok(())
    }

    #[test]
    fn segment_merge_preserves_latest_batch_receipt() -> Result<()> {
        let index = create_in_ram()?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        writer.set_merge_policy(Box::new(tantivy::merge_policy::NoMergePolicy));
        let first = Uuid::new_v4();
        let last = Uuid::new_v4();
        for (file_id, token) in [(1, first), (2, last)] {
            add_content_doc(
                &mut writer,
                &index.fields,
                &sample_doc(DocKey::from_parts(7, file_id), "file.txt", "mergedword"),
            )?;
            commit_batch(&mut writer, token)?;
        }
        let segments = index.index.searchable_segment_ids()?;
        assert_eq!(segments.len(), 2);
        let receipt = batch_receipt(&index.index)?;
        writer.merge(&segments).wait()?;
        assert_eq!(index.index.searchable_segment_ids()?.len(), 1);
        assert_eq!(committed_batch(&index.index)?, Some(last));
        assert_eq!(batch_receipt(&index.index)?, receipt);
        assert_eq!(matches(&index, index.fields.content, "mergedword")?, 2);
        Ok(())
    }

    #[test]
    fn legacy_or_invalid_receipts_do_not_claim_journal_coverage() -> Result<()> {
        let index = create_in_ram()?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        assert!(commit_batch(&mut writer, Uuid::nil()).is_err());
        assert_eq!(committed_batch(&index.index)?, None);
        commit_batch(&mut writer, Uuid::new_v4())?;
        writer.commit()?;
        assert_eq!(committed_batch(&index.index)?, None);
        let id = Uuid::new_v4();
        let legacy_receipt = BatchReceipt {
            batch_id: id,
            commit_id: Uuid::new_v4(),
            complete: true,
        };
        let mut prepared = writer.prepare_commit()?;
        prepared.set_payload(&format!(
            "ultrasearch-ingestion-v1:{}:{}:complete",
            legacy_receipt.batch_id, legacy_receipt.commit_id
        ));
        prepared.commit()?;
        assert_eq!(
            batch_outcome(&index.index)?,
            Some(BatchOutcome {
                receipt: legacy_receipt,
                deferred: Vec::new(),
            })
        );
        let first = DocKey::from_parts(7, 42);
        let second = DocKey::from_parts(7, 43);
        let over_limit = (0..=MAX_DEFERRED_KEYS)
            .map(|position| DocKey::from_parts(7, position as u64).to_string())
            .collect::<Vec<_>>()
            .join(",");
        for invalid in [
            "not-a-batch-id".to_owned(),
            id.to_string(),
            format!("ultrasearch-ingestion-v1:{}:{id}:complete", Uuid::nil()),
            format!("ultrasearch-ingestion-v1:{id}:{}:partial", Uuid::nil()),
            format!("ultrasearch-ingestion-v1:{id}:{id}:unknown"),
            format!("ultrasearch-ingestion-v1:{id}:{id}:complete:extra"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:partial:{first}"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:{first},{first}"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:{second},{first}"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:7:0x2a"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:{first},"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:{first}:extra"),
            format!("ultrasearch-ingestion-v2:{id}:{id}:complete:{over_limit}"),
            format!(
                "ultrasearch-ingestion-v2:{id}:{id}:complete:{}",
                "x".repeat(MAX_BATCH_PAYLOAD_BYTES)
            ),
        ] {
            let mut prepared = writer.prepare_commit()?;
            prepared.set_payload(&invalid);
            prepared.commit()?;
            assert!(batch_outcome(&index.index).is_err(), "{invalid}");
            assert!(batch_receipt(&index.index).is_err());
            assert!(committed_batch(&index.index).is_err());
        }
        Ok(())
    }

    #[test]
    fn deferred_receipts_commit_tombstones_survive_reopen_and_change_on_replay() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let index = open_or_create(dir.path())?;
        let cfg = WriterConfig {
            heap_size_bytes: 20_000_000,
            num_threads: 1,
        };
        let mut writer = create_writer(&index, &cfg)?;
        let blocked = DocKey::from_parts(u16::MAX, u64::MAX);
        let available = DocKey::from_parts(7, 42);
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(blocked, "blocked.txt", "obsoleteblockedword"),
        )?;
        commit_batch(&mut writer, Uuid::new_v4())?;
        delete_doc(&mut writer, &index.fields, blocked);
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(available, "available.txt", "availableword"),
        )?;
        let batch_id = Uuid::new_v4();
        commit_batch_with_deferred(&mut writer, batch_id, &[blocked])?;
        let outcome = batch_outcome(&index.index)?.unwrap();
        assert_eq!(outcome.deferred, vec![blocked]);
        assert_eq!(outcome.receipt.batch_id, batch_id);
        assert!(outcome.receipt.complete);
        assert_eq!(
            matches(&index, index.fields.content, "obsoleteblockedword")?,
            0
        );
        assert_eq!(matches(&index, index.fields.content, "availableword")?, 1);
        drop(writer);
        drop(index);

        let index = open_or_create(dir.path())?;
        assert_eq!(batch_outcome(&index.index)?, Some(outcome.clone()));
        let reader = open_reader(&index)?;
        assert!(read_file_meta(&index, &reader, blocked)?.is_none());
        assert_eq!(reader.searcher().num_docs(), 1);
        let mut writer = create_writer(&index, &cfg)?;
        commit_batch_with_deferred(&mut writer, batch_id, &[blocked])?;
        let replayed = batch_outcome(&index.index)?.unwrap();
        assert_eq!(replayed.deferred, outcome.deferred);
        assert_ne!(replayed.receipt.commit_id, outcome.receipt.commit_id);

        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(blocked, "recovered.txt", "recoveredword"),
        )?;
        commit_batch_with_deferred(&mut writer, batch_id, &[])?;
        let recovered = batch_outcome(&index.index)?.unwrap();
        assert!(recovered.deferred.is_empty());
        assert_ne!(recovered.receipt.commit_id, replayed.receipt.commit_id);
        assert_eq!(
            matches(&index, index.fields.content, "obsoleteblockedword")?,
            0
        );
        assert_eq!(matches(&index, index.fields.content, "recoveredword")?, 1);
        assert_eq!(open_reader(&index)?.searcher().num_docs(), 2);
        Ok(())
    }

    #[test]
    fn deferred_commit_limits_reject_before_mutating_commit_evidence() -> Result<()> {
        let index = create_in_ram()?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let keys: Vec<_> = (0..MAX_DEFERRED_KEYS)
            .map(|position| DocKey::from_parts(7, position as u64))
            .collect();
        let batch_id = Uuid::new_v4();
        commit_batch_with_deferred(&mut writer, batch_id, &keys)?;
        let committed = batch_outcome(&index.index)?.unwrap();
        assert_eq!(committed.deferred, keys);
        assert_eq!(committed_batch(&index.index)?, Some(batch_id));

        let mut excessive = keys.clone();
        excessive.push(DocKey::from_parts(8, 0));
        for invalid in [
            vec![keys[0], keys[0]],
            vec![keys[1], keys[0]],
            vec![DocKey(1u128 << 100)],
            excessive,
        ] {
            assert!(commit_batch_with_deferred(&mut writer, batch_id, &invalid).is_err());
            assert_eq!(batch_outcome(&index.index)?, Some(committed.clone()));
        }
        assert!(commit_batch_with_deferred(&mut writer, Uuid::nil(), &keys).is_err());
        assert_eq!(batch_outcome(&index.index)?, Some(committed));
        Ok(())
    }

    #[test]
    fn partial_and_replayed_commits_cannot_reuse_completion_evidence() -> Result<()> {
        let index = create_in_ram()?;
        let mut writer = create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let batch_id = Uuid::new_v4();
        let key = DocKey::from_parts(7, 42);
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "file.txt", "partialword"),
        )?;
        commit_partial_batch(&mut writer, batch_id)?;
        let partial = batch_receipt(&index.index)?.unwrap();
        assert_eq!(partial.batch_id, batch_id);
        assert!(!partial.complete);
        assert_eq!(committed_batch(&index.index)?, None);
        assert_eq!(matches(&index, index.fields.content, "partialword")?, 1);

        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "file.txt", "completeword"),
        )?;
        commit_batch(&mut writer, batch_id)?;
        let complete = batch_receipt(&index.index)?.unwrap();
        assert!(complete.complete);
        assert_eq!(complete.batch_id, batch_id);
        assert_ne!(complete.commit_id, partial.commit_id);
        assert_eq!(committed_batch(&index.index)?, Some(batch_id));
        assert_eq!(matches(&index, index.fields.content, "partialword")?, 0);

        // A later successful replay can contain a different live snapshot. It
        // must not validate a backup of the earlier completed attempt.
        add_content_doc(
            &mut writer,
            &index.fields,
            &sample_doc(key, "file.txt", "replayedword"),
        )?;
        commit_batch(&mut writer, batch_id)?;
        let replay = batch_receipt(&index.index)?.unwrap();
        assert_eq!(replay.batch_id, batch_id);
        assert!(replay.complete);
        assert_ne!(replay.commit_id, complete.commit_id);
        assert_eq!(matches(&index, index.fields.content, "completeword")?, 0);
        assert_eq!(matches(&index, index.fields.content, "replayedword")?, 1);

        // Failing during another replay downgrades the index to partial even
        // though a previous successful attempt of this batch once existed.
        commit_partial_batch(&mut writer, batch_id)?;
        let retried_partial = batch_receipt(&index.index)?.unwrap();
        assert!(!retried_partial.complete);
        assert_ne!(retried_partial.commit_id, replay.commit_id);
        assert_eq!(committed_batch(&index.index)?, None);
        Ok(())
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
