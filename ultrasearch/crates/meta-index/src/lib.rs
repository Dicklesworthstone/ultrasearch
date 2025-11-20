//! Metadata (filename/attributes) index built on Tantivy.
//!
//! This crate owns the schema for the "metadata" index described in the plan
//! (doc_key, volume, name, ext, size, timestamps, flags). For c00.3 we provide
//! a schema builder and a thin wrapper to open/create the index; the service
//! will wire the actual writer/reader later.

use std::path::Path;

use anyhow::Result;
use core_types::DocKey;
use tantivy::{schema::*, Index, IndexWriter};

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

    let doc_key = builder.add_u64_field("doc_key", FAST | STORED);
    let volume = builder.add_u64_field("volume", FAST | STORED);
    let name = builder.add_text_field("name", TEXT | STORED);
    let path = builder.add_text_field("path", TEXT | STORED);
    let ext = builder.add_text_field("ext", STRING | FAST);
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
        Index::open_in_dir(path)?
    } else {
        Index::create_in_dir(path, schema)?
    };
    Ok(MetaIndex { index, fields })
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
    let mut writer = meta.index.writer_with_num_threads(cfg.num_threads, cfg.heap_size_bytes)?;
    // For metadata we prefer merges that keep segment counts modest but not enormous; rely on Tantivy defaults for now.
    Ok(writer)
}

/// Open a read-only handle with minimal caching suitable for the long-lived service.
pub fn open_reader(meta: &MetaIndex) -> Result<tantivy::IndexReader> {
    let reader = meta
        .index
        .reader_builder()
        .reload_policy(tantivy::ReloadPolicy::OnCommit)
        .try_into()?;
    Ok(reader)
}

/// Convert a `MetaDoc` into a Tantivy `Document`.
pub fn to_document(doc: &MetaDoc, fields: &MetaFields) -> tantivy::Document {
    let mut d = tantivy::Document::new();
    d.add_u64(fields.doc_key, doc.key.0);
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
    fn schema_builds_and_fields_present() {
        let (schema, fields) = build_schema();
        assert!(schema.get_field_entry(fields.doc_key).is_stored());
        assert!(schema.get_field_entry(fields.name).is_text());
    }

    #[test]
    fn create_index_in_ram() {
        let (schema, _fields) = build_schema();
        let dir = RamDirectory::create();
        let _index = Index::create(dir, schema).expect("index should create in ram");
    }
}
