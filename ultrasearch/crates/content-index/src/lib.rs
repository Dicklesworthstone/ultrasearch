//! Tantivy-based content index (full-text).
//!
//! Schema matches the plan: doc_key, volume, name/path/ext metadata, size,
//! modified, optional content_lang, and the main `content` text field.

use std::path::Path;

use anyhow::Result;
use core_types::DocKey;
use tantivy::{
    schema::*,
    Index, IndexWriter, ReloadPolicy,
};

/// Field handles for the content index schema.
#[derive(Debug, Clone)]
pub struct ContentFields {
    pub doc_key: Field,
    pub volume: Field,
    pub name: Field,
    pub path: Field,
    pub ext: Field,
    pub size: Field,
    pub modified: Field,
    pub content_lang: Field,
    pub content: Field,
}

pub fn build_schema() -> (Schema, ContentFields) {
    let mut builder = Schema::builder();

    let doc_key = builder.add_u64_field("doc_key", FAST | STORED);
    let volume = builder.add_u64_field("volume", FAST | STORED);
    let name = builder.add_text_field("name", TEXT | STORED);
    let path = builder.add_text_field("path", TEXT | STORED);
    let ext = builder.add_text_field("ext", STRING | FAST);
    let size = builder.add_u64_field("size", FAST | STORED);
    let modified = builder.add_i64_field("modified", FAST | STORED);
    let content_lang = builder.add_text_field("content_lang", STRING | STORED);
    let content = builder.add_text_field("content", TEXT);

    let fields = ContentFields {
        doc_key,
        volume,
        name,
        path,
        ext,
        size,
        modified,
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

pub fn open_or_create(path: &Path) -> Result<ContentIndex> {
    let (schema, fields) = build_schema();
    let index = if path.join("meta.json").exists() {
        Index::open_in_dir(path)?
    } else {
        Index::create_in_dir(path, schema)?
    };
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
        .reload_policy(ReloadPolicy::OnCommit)
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
    pub modified: i64,
    pub content_lang: Option<String>,
    pub content: String,
}

pub fn to_document(doc: &ContentDoc, fields: &ContentFields) -> tantivy::Document {
    let mut d = tantivy::Document::new();
    d.add_u64(fields.doc_key, doc.key.0);
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
    d.add_i64(fields.modified, doc.modified);
    if let Some(lang) = &doc.content_lang {
        d.add_text(fields.content_lang, lang);
    }
    d.add_text(fields.content, &doc.content);
    d
}
