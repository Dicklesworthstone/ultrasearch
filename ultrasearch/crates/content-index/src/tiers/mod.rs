use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::Query;
use tantivy::schema::Document;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher};

use crate::{ContentFields, build_schema};

/// A tiered index managing a hot (RAM) and cold (Disk) layer.
pub struct TieredIndex {
    pub hot: Index,
    pub cold: Index,
    hot_writer: Arc<std::sync::Mutex<IndexWriter>>,
    cold_writer: Arc<std::sync::Mutex<IndexWriter>>,
    hot_reader: IndexReader,
    cold_reader: IndexReader,
    fields: ContentFields,
}

impl TieredIndex {
    pub fn open_or_create(cold_path: &Path) -> Result<Self> {
        let (schema, fields) = build_schema();

        // Hot index in RAM
        let hot = Index::create_in_ram(schema.clone());
        let hot_writer = hot.writer(50_000_000)?;
        
        // Cold index on disk
        let cold = if cold_path.exists() {
            Index::open_in_dir(cold_path)?
        } else {
            std::fs::create_dir_all(cold_path)?;
            Index::create_in_dir(cold_path, schema)?
        };
        let cold_writer = cold.writer(100_000_000)?;

        let hot_reader = hot
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
            
        let cold_reader = cold
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        Ok(Self {
            hot,
            cold,
            hot_writer: Arc::new(std::sync::Mutex::new(hot_writer)),
            cold_writer: Arc::new(std::sync::Mutex::new(cold_writer)),
            hot_reader,
            cold_reader,
            fields,
        })
    }

    pub fn add_doc<D: Document>(&self, doc: D) -> Result<()> {
        let mut w = self.hot_writer.lock().unwrap();
        w.add_document(doc)?;
        w.commit()?; // Auto-commit for hot tier? Or caller controls?
        // For low latency, we commit frequently.
        Ok(())
    }

    /// Merge hot segment into cold.
    /// Note: This is a simplification. Real compaction moves docs and deletes from hot.
    pub fn compact(&self) -> Result<()> {
        // 1. Identify docs in hot
        // 2. Move to cold
        // 3. Delete from hot
        // Since Tantivy doesn't support moving docs between indices easily without re-indexing,
        // we effectively re-index.
        
        // For P3 task, we'll implement a stub or simple re-indexer.
        Ok(())
    }

    pub fn search(&self, query: &dyn Query, limit: usize) -> Result<Vec<(f32, String)>> {
        // Search both
        let hot_searcher = self.hot_reader.searcher();
        let cold_searcher = self.cold_reader.searcher();

        // We can't easily merge results from two searchers with global scoring.
        // But we can get top N from both and merge.
        
        let hot_top = hot_searcher.search(query, &TopDocs::with_limit(limit))?;
        let cold_top = cold_searcher.search(query, &TopDocs::with_limit(limit))?;

        // Merge and sort
        // Return simpler result for now
        Ok(vec![]) 
    }
}
