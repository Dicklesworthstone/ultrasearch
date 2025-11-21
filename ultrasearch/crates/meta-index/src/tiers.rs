use crate::{MetaDoc, MetaFields, MetaIndex, build_schema, to_document};
use anyhow::Result;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::Query;
use tantivy::schema::Document;
use tantivy::schema::TantivyDocument;
use tantivy::schema::Value;
use tantivy::{DocAddress, Index, IndexWriter};

/// A multi-tier index managing an in-memory "delta" tier and a persistent "cold" tier.
pub struct TieredMetaIndex {
    delta: MetaIndex,
    cold: MetaIndex,
    delta_writer: IndexWriter,
}

impl TieredMetaIndex {
    pub fn new(cold_path: &Path) -> Result<Self> {
        // 1. Open/Create Cold Index (Disk)
        let (schema, fields) = build_schema();
        let cold_index = if cold_path.join("meta.json").exists() {
            Index::open_in_dir(cold_path)?
        } else {
            Index::create_in_dir(cold_path, schema.clone())?
        };
        let cold = MetaIndex {
            index: cold_index,
            fields: fields.clone(),
        };

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
        })
    }

    pub fn add_doc(&mut self, doc: MetaDoc) -> Result<()> {
        let tdoc = to_document(&doc, &self.delta.fields);
        self.delta_writer.add_document(tdoc)?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<u64> {
        Ok(self.delta_writer.commit()?)
    }

    /// Merge delta index into cold index.
    pub fn compact(&mut self) -> Result<()> {
        self.delta_writer.commit()?;

        let delta_reader = self.delta.index.reader()?;
        let searcher = delta_reader.searcher();

        // Open cold writer
        let mut cold_writer = self.cold.index.writer(100_000_000)?;

        // Iterate all docs in delta using searcher
        for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
            for doc_id in segment_reader.doc_ids_alive() {
                let doc: TantivyDocument =
                    searcher.doc(DocAddress::new(segment_ord as u32, doc_id))?;
                cold_writer.add_document(doc)?;
            }
        }

        cold_writer.commit()?;

        // Clear delta
        self.delta_writer.delete_all_documents()?;
        self.delta_writer.commit()?;

        Ok(())
    }

    /// Search both tiers and merge results.
    pub fn search(&self, query: &dyn Query, limit: usize) -> Result<Vec<(f32, MetaDoc)>> {
        let delta_reader = self.delta.index.reader()?;
        let cold_reader = self.cold.index.reader()?;

        let delta_searcher = delta_reader.searcher();
        let cold_searcher = cold_reader.searcher();

        let top_delta = delta_searcher.search(query, &TopDocs::with_limit(limit))?;
        let top_cold = cold_searcher.search(query, &TopDocs::with_limit(limit))?;

        let mut results = Vec::new();

        for (score, addr) in top_delta {
            let doc: TantivyDocument = delta_searcher.doc(addr)?;
            if let Some(md) = doc_to_meta(&doc, &self.delta.fields) {
                results.push((score, md));
            }
        }

        for (score, addr) in top_cold {
            let doc: TantivyDocument = cold_searcher.doc(addr)?;
            if let Some(md) = doc_to_meta(&doc, &self.cold.fields) {
                results.push((score, md));
            }
        }

        // Sort and Dedup
        results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut dedup = std::collections::HashMap::new();
        for (score, doc) in results {
            dedup.entry(doc.key).or_insert((score, doc));
        }

        let mut final_res: Vec<_> = dedup.into_values().collect();
        final_res.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        final_res.truncate(limit);

        Ok(final_res)
    }
}

// Helper to reverse mapping (Document -> MetaDoc)
fn doc_to_meta(doc: &TantivyDocument, fields: &MetaFields) -> Option<MetaDoc> {
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
            f if f == fields.doc_key => key = value.as_u64().map(core_types::DocKey),
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
