use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use core_types::DocKey;
use tantivy::collector::TopDocs;
use tantivy::query::Query;
use tantivy::schema::{Document, TantivyDocument, Value};
use tantivy::{DocAddress, Index, IndexReader, IndexWriter, ReloadPolicy, Term};

use crate::{ContentFields, build_schema, delete_doc};

/// A tiered index managing a hot (RAM) and cold (Disk) layer.
pub struct TieredIndex {
    pub hot: Index,
    pub cold: Index,
    hot_writer: Arc<Mutex<IndexWriter>>,
    cold_writer: Arc<Mutex<IndexWriter>>,
    hot_reader: IndexReader,
    cold_reader: IndexReader,
    fields: ContentFields,
    deleted: Arc<Mutex<HashSet<DocKey>>>,
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
            hot_writer: Arc::new(Mutex::new(hot_writer)),
            cold_writer: Arc::new(Mutex::new(cold_writer)),
            hot_reader,
            cold_reader,
            fields,
            deleted: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn fields(&self) -> &ContentFields {
        &self.fields
    }

    pub fn add_doc<D: Document>(&self, doc: D) -> Result<()> {
        let mut w = self
            .hot_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("hot writer lock poisoned"))?;
        w.add_document(doc)?;
        w.commit()?;
        self.hot_reader.reload()?;
        Ok(())
    }

    pub fn delete_doc(&self, key: DocKey) -> Result<()> {
        {
            let mut del = self
                .deleted
                .lock()
                .map_err(|_| anyhow::anyhow!("deleted set lock poisoned"))?;
            del.insert(key);
        }
        let mut w = self
            .hot_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("hot writer lock poisoned"))?;
        delete_doc(&mut w, &self.fields, key);
        w.commit()?;
        self.hot_reader.reload()?;
        Ok(())
    }

    /// Merge hot tier documents and pending deletion tombstones into cold tier.
    pub fn compact(&self) -> Result<()> {
        let mut hot_w = self
            .hot_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("hot writer lock poisoned"))?;
        let mut cold_w = self
            .cold_writer
            .lock()
            .map_err(|_| anyhow::anyhow!("cold writer lock poisoned"))?;
        let mut deleted = self
            .deleted
            .lock()
            .map_err(|_| anyhow::anyhow!("deleted set lock poisoned"))?;

        hot_w.commit()?;
        self.hot_reader.reload()?;
        let hot_searcher = self.hot_reader.searcher();

        for &key in deleted.iter() {
            delete_doc(&mut cold_w, &self.fields, key);
        }

        for (segment_ord, segment_reader) in hot_searcher.segment_readers().iter().enumerate() {
            for doc_id in segment_reader.doc_ids_alive() {
                let doc: TantivyDocument =
                    hot_searcher.doc(DocAddress::new(segment_ord as u32, doc_id))?;
                if let Some(key_str) = doc
                    .get_first(self.fields.doc_key)
                    .and_then(|v| v.as_str())
                {
                    if let Ok(dk) = key_str.parse::<DocKey>()
                        && deleted.contains(&dk)
                    {
                        continue;
                    }
                    cold_w.delete_term(Term::from_field_text(self.fields.doc_key, key_str));
                }
                cold_w.add_document(doc)?;
            }
        }

        cold_w.commit()?;
        self.cold_reader.reload()?;

        hot_w.delete_all_documents()?;
        hot_w.commit()?;
        self.hot_reader.reload()?;
        deleted.clear();

        Ok(())
    }

    pub fn search(&self, query: &dyn Query, limit: usize) -> Result<Vec<(f32, String)>> {
        let hot_searcher = self.hot_reader.searcher();
        let cold_searcher = self.cold_reader.searcher();
        let deleted = self
            .deleted
            .lock()
            .map_err(|_| anyhow::anyhow!("deleted set lock poisoned"))?;

        let hot_top = hot_searcher.search(query, &TopDocs::with_limit(limit))?;
        let cold_top = cold_searcher.search(query, &TopDocs::with_limit(limit))?;

        let mut dedup: HashMap<String, (f32, String)> = HashMap::new();

        for (score, addr) in hot_top {
            let doc: TantivyDocument = hot_searcher.doc(addr)?;
            if let Some((key_id, label)) = self.extract_doc_identity(&doc, &deleted) {
                dedup.entry(key_id).or_insert((score, label));
            }
        }

        for (score, addr) in cold_top {
            let doc: TantivyDocument = cold_searcher.doc(addr)?;
            if let Some((key_id, label)) = self.extract_doc_identity(&doc, &deleted) {
                dedup.entry(key_id).or_insert((score, label));
            }
        }

        let mut merged: Vec<(f32, String)> = dedup.into_values().collect();
        merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        merged.truncate(limit);
        Ok(merged)
    }

    fn extract_doc_identity(
        &self,
        doc: &TantivyDocument,
        deleted: &HashSet<DocKey>,
    ) -> Option<(String, String)> {
        let key_str = doc
            .get_first(self.fields.doc_key)
            .and_then(|v| v.as_str())?;
        if let Ok(dk) = key_str.parse::<DocKey>()
            && deleted.contains(&dk)
        {
            return None;
        }
        let label = doc
            .get_first(self.fields.path)
            .and_then(|v| v.as_str())
            .or_else(|| doc.get_first(self.fields.name).and_then(|v| v.as_str()))
            .unwrap_or(key_str)
            .to_string();
        Some((key_str.to_string(), label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentDoc, doc_to_tantivy};
    use tantivy::query::AllQuery;
    use tempfile::tempdir;

    #[test]
    fn tiered_content_index_searches_compacts_and_honors_deletions() -> Result<()> {
        let dir = tempdir()?;
        let tiered = TieredIndex::open_or_create(dir.path())?;

        let key1 = DocKey::from_parts(1, 10);
        let key2 = DocKey::from_parts(1, 20);
        tiered.add_doc(doc_to_tantivy(
            tiered.fields(),
            &ContentDoc {
                key: key1,
                volume: 1,
                name: "one.txt".into(),
                path: "C:\\docs\\one.txt".into(),
                ext: "txt".into(),
                size: 12,
                modified: 100,
                flags: 0,
                content: "alpha beta".into(),
                lang: None,
            },
        ))?;
        tiered.add_doc(doc_to_tantivy(
            tiered.fields(),
            &ContentDoc {
                key: key2,
                volume: 1,
                name: "two.txt".into(),
                path: "C:\\docs\\two.txt".into(),
                ext: "txt".into(),
                size: 15,
                modified: 100,
                flags: 0,
                content: "gamma delta".into(),
                lang: None,
            },
        ))?;

        assert_eq!(tiered.search(&AllQuery, 10)?.len(), 2);
        tiered.compact()?;
        assert_eq!(tiered.search(&AllQuery, 10)?.len(), 2);

        tiered.delete_doc(key1)?;
        let hits = tiered.search(&AllQuery, 10)?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "C:\\docs\\two.txt");

        tiered.compact()?;
        let hits_after = tiered.search(&AllQuery, 10)?;
        assert_eq!(hits_after.len(), 1);
        assert_eq!(hits_after[0].1, "C:\\docs\\two.txt");
        Ok(())
    }
}
