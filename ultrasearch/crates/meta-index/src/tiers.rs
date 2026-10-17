use crate::{
    MetaDoc, MetaFields, MetaIndex, add_batch, build_schema, delete_doc, open_or_create_index,
};
use anyhow::Result;
use core_types::DocKey;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::Query;
use tantivy::schema::{Document, TantivyDocument, Value};
use tantivy::{DocAddress, Index, IndexWriter};

/// A multi-tier index managing an in-memory "delta" tier and a persistent "cold" tier.
pub struct TieredMetaIndex {
    delta: MetaIndex,
    cold: MetaIndex,
    delta_writer: IndexWriter,
    deleted: HashSet<DocKey>,
}

impl TieredMetaIndex {
    pub fn new(cold_path: &Path) -> Result<Self> {
        // 1. Open/Create Cold Index (Disk)
        let (schema, fields) = build_schema();
        let cold = open_or_create_index(cold_path)?;

        // 2. Create Delta Index (RAM)
        let ram_dir = tantivy::directory::RamDirectory::create();
        let delta_index = Index::create(ram_dir, schema, tantivy::IndexSettings::default())?;
        let delta = MetaIndex {
            index: delta_index,
            fields,
        };

        // 3. Prepare writers
        // Delta writer is always active for ingestion
        let delta_writer = delta.index.writer(50_000_000)?; // 50MB heap for delta

        Ok(Self {
            delta,
            cold,
            delta_writer,
            deleted: HashSet::new(),
        })
    }

    pub fn add_doc(&mut self, doc: MetaDoc) -> Result<()> {
        self.deleted.remove(&doc.key);
        delete_doc(&mut self.delta_writer, &self.delta.fields, doc.key);
        add_batch(&mut self.delta_writer, &self.delta.fields, [doc])?;
        Ok(())
    }

    pub fn delete_doc(&mut self, key: DocKey) {
        delete_doc(&mut self.delta_writer, &self.delta.fields, key);
        self.deleted.insert(key);
    }

    pub fn commit(&mut self) -> Result<u64> {
        Ok(self.delta_writer.commit()?)
    }

    /// Merge delta index and pending deletion tombstones into cold index.
    pub fn compact(&mut self) -> Result<()> {
        self.delta_writer.commit()?;

        let delta_reader = self.delta.index.reader()?;
        let searcher = delta_reader.searcher();

        // Open cold writer
        let mut cold_writer = self.cold.index.writer(100_000_000)?;

        for &deleted_key in &self.deleted {
            delete_doc(&mut cold_writer, &self.cold.fields, deleted_key);
        }

        // Iterate all docs in delta using searcher
        for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
            for doc_id in segment_reader.doc_ids_alive() {
                let doc: TantivyDocument =
                    searcher.doc(DocAddress::new(segment_ord as u32, doc_id))?;
                if let Some(meta) = doc_to_meta(&doc, &self.delta.fields) {
                    if self.deleted.contains(&meta.key) {
                        continue;
                    }
                    delete_doc(&mut cold_writer, &self.cold.fields, meta.key);
                }
                cold_writer.add_document(doc)?;
            }
        }

        cold_writer.commit()?;

        // Clear delta and tombstones
        self.delta_writer.delete_all_documents()?;
        self.delta_writer.commit()?;
        self.deleted.clear();

        Ok(())
    }

    /// Search both tiers and merge results, honoring delta overrides and deletion tombstones.
    pub fn search(&self, query: &dyn Query, limit: usize) -> Result<Vec<(f32, MetaDoc)>> {
        let delta_reader = self.delta.index.reader()?;
        let cold_reader = self.cold.index.reader()?;

        let delta_searcher = delta_reader.searcher();
        let cold_searcher = cold_reader.searcher();

        let top_delta = delta_searcher.search(query, &TopDocs::with_limit(limit))?;
        let top_cold = cold_searcher.search(query, &TopDocs::with_limit(limit))?;

        let mut dedup: HashMap<DocKey, (f32, MetaDoc)> = HashMap::new();

        for (score, addr) in top_delta {
            let doc: TantivyDocument = delta_searcher.doc(addr)?;
            if let Some(md) = doc_to_meta(&doc, &self.delta.fields)
                && !self.deleted.contains(&md.key)
            {
                dedup.entry(md.key).or_insert((score, md));
            }
        }

        for (score, addr) in top_cold {
            let doc: TantivyDocument = cold_searcher.doc(addr)?;
            if let Some(md) = doc_to_meta(&doc, &self.cold.fields)
                && !self.deleted.contains(&md.key)
            {
                dedup.entry(md.key).or_insert((score, md));
            }
        }

        let mut final_res: Vec<_> = dedup.into_values().collect();
        final_res.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        final_res.truncate(limit);

        Ok(final_res)
    }
}

// Helper to reverse mapping (Document -> MetaDoc)
pub fn doc_to_meta(doc: &TantivyDocument, fields: &MetaFields) -> Option<MetaDoc> {
    let mut key = None;
    let mut name = None;
    let mut path = None;
    let mut ext = None;
    let mut size = None;
    let mut created = None;
    let mut modified = None;
    let mut flags = None;
    let mut volume = None;

    for (field, value) in doc.iter_fields_and_values() {
        match field {
            f if f == fields.doc_key => key = value.as_str().and_then(|value| value.parse().ok()),
            f if f == fields.volume => volume = value.as_u64().map(|v| v as u16),
            f if f == fields.name => name = value.as_str().map(|s| s.to_string()),
            f if f == fields.path => path = value.as_str().map(|s| s.to_string()),
            f if f == fields.ext => ext = value.as_str().map(|s| s.to_string()),
            f if f == fields.size => size = value.as_u64(),
            f if f == fields.created => created = value.as_i64(),
            f if f == fields.modified => modified = value.as_i64(),
            f if f == fields.flags => flags = value.as_u64(),
            _ => {}
        }
    }

    if let (Some(k), Some(v), Some(n), Some(s), Some(c), Some(m), Some(f)) =
        (key, volume, name, size, created, modified, flags)
    {
        Some(MetaDoc {
            key: k,
            volume: v,
            name: n,
            path,
            ext,
            size: s,
            created: c,
            modified: m,
            flags: f,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::query::AllQuery;
    use tempfile::tempdir;

    #[test]
    fn tiered_meta_index_preserves_deletions_before_and_after_compaction() -> Result<()> {
        let dir = tempdir()?;
        let mut index = TieredMetaIndex::new(dir.path())?;

        let key1 = DocKey::from_parts(1, 100);
        let key2 = DocKey::from_parts(1, 200);
        index.add_doc(MetaDoc {
            key: key1,
            volume: 1,
            name: "alpha.rs".into(),
            path: Some("C:\\src\\alpha.rs".into()),
            ext: Some("rs".into()),
            size: 100,
            created: 1_700_000_000,
            modified: 1_700_000_000,
            flags: 0,
        })?;
        index.add_doc(MetaDoc {
            key: key2,
            volume: 1,
            name: "beta.rs".into(),
            path: Some("C:\\src\\beta.rs".into()),
            ext: Some("rs".into()),
            size: 200,
            created: 1_700_000_000,
            modified: 1_700_000_000,
            flags: 0,
        })?;
        index.compact()?;

        // Both docs are now in the cold tier. Delete key1.
        index.delete_doc(key1);
        index.commit()?;

        // Before compaction, key1 must already be suppressed from search results.
        let hits_before = index.search(&AllQuery, 10)?;
        assert_eq!(hits_before.len(), 1);
        assert_eq!(hits_before[0].1.key, key2);

        // After compaction, key1 must be permanently removed from the cold tier.
        index.compact()?;
        let hits_after = index.search(&AllQuery, 10)?;
        assert_eq!(hits_after.len(), 1);
        assert_eq!(hits_after[0].1.key, key2);

        Ok(())
    }
}
