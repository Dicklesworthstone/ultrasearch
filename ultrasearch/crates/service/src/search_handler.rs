use anyhow::Result;
use content_index::{ContentIndex, open_or_create as open_content};
use ipc::{
    FieldKind, QueryExpr, SearchHit, SearchMode, SearchRequest, SearchResponse, TermExpr,
    TermModifier,
};
use meta_index::{MetaFields, MetaIndex, open_or_create_index, open_reader};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Document, IndexRecordOption, TantivyDocument, Value};
use tantivy::{IndexReader, Score, Term};
use tracing::warn;

/// Trait for handling search requests.
pub trait SearchHandler: Send + Sync {
    fn search(&self, req: SearchRequest) -> SearchResponse;
}

/// Simple placeholder handler that returns an empty response.
#[derive(Debug, Default)]
pub struct StubSearchHandler;

impl SearchHandler for StubSearchHandler {
    fn search(&self, req: SearchRequest) -> SearchResponse {
        SearchResponse {
            id: req.id,
            hits: Vec::new(),
            total: 0,
            truncated: false,
            took_ms: 0,
            served_by: Some("service-stub".into()),
        }
    }
}

/// Handler backed by metadata and optional content index.
pub struct UnifiedSearchHandler {
    meta: MetaIndex,
    meta_reader: IndexReader,
    content: Option<(ContentIndex, IndexReader)>,
}

impl UnifiedSearchHandler {
    pub fn try_new(meta_path: &Path, content_path: &Path) -> Result<Self> {
        tracing::info!(
            "UnifiedSearchHandler: opening meta index at {:?}",
            meta_path
        );
        let meta = open_or_create_index(meta_path)?;
        let meta_reader = open_reader(&meta)?;

        let content = match open_content(content_path) {
            Ok(idx) => {
                tracing::info!(
                    "UnifiedSearchHandler: opened content index at {:?}",
                    content_path
                );
                let reader = content_index::open_reader(&idx)?;
                Some((idx, reader))
            }
            Err(e) => {
                warn!("failed to open content index at {:?}: {}", content_path, e);
                None
            }
        };

        Ok(Self {
            meta,
            meta_reader,
            content,
        })
    }

    fn build_meta_query(&self, expr: &QueryExpr) -> Result<Box<dyn Query>> {
        self.build_query(expr, &self.meta.fields, &self.meta.index)
    }

    fn build_query(
        &self,
        expr: &QueryExpr,
        fields: &MetaFields,
        index: &tantivy::Index,
    ) -> Result<Box<dyn Query>> {
        Ok(match expr {
            QueryExpr::Term(t) => self.term_query(t, fields, index)?,
            QueryExpr::Range(_) => Box::new(BooleanQuery::new(vec![])),
            QueryExpr::Not(inner) => Box::new(BooleanQuery::new(vec![(
                Occur::MustNot,
                self.build_query(inner, fields, index)?,
            )])),
            QueryExpr::And(items) => Box::new(BooleanQuery::new(
                items
                    .iter()
                    .map(|q| Ok((Occur::Must, self.build_query(q, fields, index)?)))
                    .collect::<Result<Vec<_>>>()?,
            )),
            QueryExpr::Or(items) => Box::new(BooleanQuery::new(
                items
                    .iter()
                    .map(|q| Ok((Occur::Should, self.build_query(q, fields, index)?)))
                    .collect::<Result<Vec<_>>>()?,
            )),
        })
    }

    fn term_query(
        &self,
        term: &TermExpr,
        fields: &MetaFields,
        index: &tantivy::Index,
    ) -> Result<Box<dyn Query>> {
        let value = term.value.trim();
        if value.is_empty() {
            return Ok(Box::new(BooleanQuery::new(vec![])));
        }

        let target_fields: Vec<FieldKind> = match term.field {
            Some(f) => vec![f],
            None => vec![FieldKind::Name, FieldKind::Path],
        };

        let mut clauses = Vec::new();
        for field in target_fields {
            match field {
                FieldKind::Ext => {
                    let t = Term::from_field_text(fields.ext, value);
                    clauses.push((
                        Occur::Should,
                        Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs)) as Box<dyn Query>,
                    ));
                }
                FieldKind::Name | FieldKind::Path => match term.modifier {
                    TermModifier::Prefix => {
                        let pf = if matches!(field, FieldKind::Name) {
                            fields.name
                        } else {
                            fields.path
                        };
                        // Fallback for PrefixQuery removal
                        let t = Term::from_field_text(pf, value);
                        clauses.push((
                            Occur::Should,
                            Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
                                as Box<dyn Query>,
                        ));
                    }
                    _ => {
                        let mut parser = QueryParser::for_index(
                            index,
                            vec![if matches!(field, FieldKind::Name) {
                                fields.name
                            } else {
                                fields.path
                            }],
                        );
                        parser.set_conjunction_by_default();
                        if let Ok(q) = parser.parse_query(value) {
                            clauses.push((Occur::Should, q));
                        }
                    }
                },
                _ => {}
            }
        }

        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn build_content_query(&self, expr: &QueryExpr) -> Result<Box<dyn Query>> {
        if let Some((idx, _)) = &self.content {
            // For content query, default fields might include content + name/path
            // We can map QueryExpr fields to ContentFields
            // This requires a specialized build_query or mapping logic.
            // For simplicity, we reuse build_query but we need to map MetaFields-like structure or make build_query generic.
            // Since MetaFields and ContentFields are different structs, we can't pass them easily unless we have a trait or adapter.
            // But term_query matches on FieldKind. We can just reimplement term_query for ContentFields.

            Ok(match expr {
                QueryExpr::Term(t) => self.term_query_content(t, &idx.fields, &idx.index)?,
                QueryExpr::Range(_) => Box::new(BooleanQuery::new(vec![])),
                QueryExpr::Not(inner) => Box::new(BooleanQuery::new(vec![(
                    Occur::MustNot,
                    self.build_content_query(inner)?,
                )])),
                QueryExpr::And(items) => Box::new(BooleanQuery::new(
                    items
                        .iter()
                        .map(|q| Ok((Occur::Must, self.build_content_query(q)?)))
                        .collect::<Result<Vec<_>>>()?,
                )),
                QueryExpr::Or(items) => Box::new(BooleanQuery::new(
                    items
                        .iter()
                        .map(|q| Ok((Occur::Should, self.build_content_query(q)?)))
                        .collect::<Result<Vec<_>>>()?,
                )),
            })
        } else {
            Err(anyhow::anyhow!("content index not available"))
        }
    }

    fn term_query_content(
        &self,
        term: &TermExpr,
        fields: &content_index::ContentFields,
        index: &tantivy::Index,
    ) -> Result<Box<dyn Query>> {
        let value = term.value.trim();
        if value.is_empty() {
            return Ok(Box::new(BooleanQuery::new(vec![])));
        }

        let target_fields: Vec<FieldKind> = match term.field {
            Some(f) => vec![f],
            None => vec![FieldKind::Name, FieldKind::Content], // Default to Name + Content
        };

        let mut clauses = Vec::new();
        for field in target_fields {
            // Map FieldKind to tantivy::schema::Field in ContentFields
            let t_field = match field {
                FieldKind::Name => Some(fields.name),
                FieldKind::Path => Some(fields.path),
                FieldKind::Ext => Some(fields.ext),
                FieldKind::Content => Some(fields.content),
                // Other fields like size/modified handled in ranges or ignored for text search
                _ => None,
            };

            if let Some(tf) = t_field {
                match term.modifier {
                    TermModifier::Prefix => {
                        let t = Term::from_field_text(tf, value);
                        clauses.push((
                            Occur::Should,
                            Box::new(TermQuery::new(t, IndexRecordOption::WithFreqs))
                                as Box<dyn Query>,
                        ));
                    }
                    _ => {
                        let mut parser = QueryParser::for_index(index, vec![tf]);
                        parser.set_conjunction_by_default();
                        if let Ok(q) = parser.parse_query(value) {
                            clauses.push((Occur::Should, q));
                        }
                    }
                }
            }
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn search_meta(&self, req: &SearchRequest) -> SearchResponse {
        let start = Instant::now();
        let limit = req.limit.max(1) as usize;
        let offset = req.offset as usize;

        let searcher = self.meta_reader.searcher();
        let query = match self.build_meta_query(&req.query) {
            Ok(q) => q,
            Err(err) => {
                warn!(error = %err, "failed to build meta query");
                return StubSearchHandler.search(req.clone());
            }
        };

        tracing::info!("executing meta query: {:?}", query);

        let top_k = limit.saturating_add(offset);
        let (hits, total) = match searcher.search(&query, &(TopDocs::with_limit(top_k), Count)) {
            Ok(r) => r,
            Err(err) => {
                warn!(error = %err, "meta search execution failed");
                return StubSearchHandler.search(req.clone());
            }
        };

        tracing::info!(
            "meta search found {} total hits (returned {})",
            total,
            hits.len()
        );

        let out = hits
            .into_iter()
            .skip(offset)
            .filter_map(|(score, addr)| {
                let retrieved = searcher.doc::<TantivyDocument>(addr).ok()?;
                to_hit(&retrieved, &self.meta.fields, score)
            })
            .collect();

        SearchResponse {
            id: req.id,
            hits: out,
            total: total as u64,
            truncated: false, // MVP
            took_ms: start.elapsed().as_millis().min(u32::MAX as u128) as u32,
            served_by: None,
        }
    }

    fn search_content(&self, req: &SearchRequest) -> SearchResponse {
        let Some((content_idx, reader)) = &self.content else {
            return StubSearchHandler.search(req.clone());
        };

        let start = Instant::now();
        let limit = req.limit.max(1) as usize;
        let offset = req.offset as usize;

        let searcher = reader.searcher();
        let query = match self.build_content_query(&req.query) {
            Ok(q) => q,
            Err(err) => {
                warn!(error = %err, "failed to build content query");
                return StubSearchHandler.search(req.clone());
            }
        };

        let top_k = limit.saturating_add(offset);
        let (hits, total) = match searcher.search(&query, &(TopDocs::with_limit(top_k), Count)) {
            Ok(r) => r,
            Err(err) => {
                warn!(error = %err, "content search execution failed");
                return StubSearchHandler.search(req.clone());
            }
        };

        let out = hits
            .into_iter()
            .skip(offset)
            .filter_map(|(score, addr)| {
                let retrieved = searcher.doc::<TantivyDocument>(addr).ok()?;
                // We need to_hit equivalent for content fields
                to_hit_content(&retrieved, &content_idx.fields, score)
            })
            .collect();

        SearchResponse {
            id: req.id,
            hits: out,
            total: total as u64,
            truncated: false,
            took_ms: start.elapsed().as_millis().min(u32::MAX as u128) as u32,
            served_by: None,
        }
    }

    fn search_hybrid(&self, req: &SearchRequest) -> SearchResponse {
        // Parallel execution? For MVP, sequential.
        // 1. Meta search
        // 2. Content search
        // 3. Merge by DocKey

        let start = Instant::now();
        let limit = req.limit.max(1) as usize;

        // Fetch more to allow merging
        let fetch_limit = limit * 2;

        // Create sub-requests
        let mut meta_req = req.clone();
        meta_req.limit = fetch_limit as u32;
        meta_req.offset = 0; // We handle paging after merge? Or simple approach: no deep paging in hybrid for now.

        let meta_resp = self.search_meta(&meta_req);

        let mut hits_map: std::collections::HashMap<core_types::DocKey, SearchHit> =
            std::collections::HashMap::new();

        for hit in meta_resp.hits {
            hits_map.insert(hit.key, hit);
        }

        if self.content.is_some() {
            let mut content_req = req.clone();
            content_req.limit = fetch_limit as u32;
            content_req.offset = 0;
            let content_resp = self.search_content(&content_req);

            for hit in content_resp.hits {
                hits_map
                    .entry(hit.key)
                    .and_modify(|e| {
                        e.score = e.score.max(hit.score); // Max score strategy? Or sum? Max is safer for boolean queries.
                        if e.snippet.is_none() {
                            e.snippet = hit.snippet.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }

        let mut merged: Vec<SearchHit> = hits_map.into_values().collect();
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let offset = req.offset as usize;
        let total = merged.len();
        let hits = merged.into_iter().skip(offset).take(limit).collect();

        SearchResponse {
            id: req.id,
            hits,
            total: total as u64, // Approx
            truncated: false,
            took_ms: start.elapsed().as_millis().min(u32::MAX as u128) as u32,
            served_by: None,
        }
    }
}

impl SearchHandler for UnifiedSearchHandler {
    fn search(&self, req: SearchRequest) -> SearchResponse {
        match req.mode {
            SearchMode::NameOnly => self.search_meta(&req),
            SearchMode::Content => self.search_content(&req),
            SearchMode::Hybrid | SearchMode::Auto => self.search_hybrid(&req),
        }
    }
}

// Helper to map content doc to SearchHit
fn to_hit_content<D: Document>(
    doc: &D,
    fields: &content_index::ContentFields,
    score: Score,
) -> Option<SearchHit> {
    let mut key = None;
    let mut name = None;
    let mut path = None;
    let mut ext = None;
    let mut size = None;
    let mut modified = None;
    let snippet = None; // TODO: snippet generation

    for (field, value) in doc.iter_fields_and_values() {
        match field {
            f if f == fields.doc_key => {
                if let Some(v) = value.as_u64() {
                    key = Some(core_types::DocKey(v));
                }
            }
            f if f == fields.name => name = value.as_str().map(|s| s.to_string()),
            f if f == fields.path => path = value.as_str().map(|s| s.to_string()),
            f if f == fields.ext => ext = value.as_str().map(|s| s.to_string()),
            f if f == fields.size => size = value.as_u64(),
            f if f == fields.modified => modified = value.as_i64(),
            // TODO: snippet from content field
            _ => {}
        }
    }

    key.map(|doc_key| SearchHit {
        key: doc_key,
        score,
        name,
        path,
        ext,
        size,
        modified,
        snippet,
    })
}

static HANDLER: OnceLock<Box<dyn SearchHandler>> = OnceLock::new();

pub fn set_search_handler(handler: Box<dyn SearchHandler>) {
    tracing::info!("Global search handler installed.");
    let _ = HANDLER.set(handler);
}

pub fn search(req: SearchRequest) -> SearchResponse {
    tracing::info!("Received search request id={} mode={:?}", req.id, req.mode);
    if let Some(h) = HANDLER.get() {
        h.search(req)
    } else {
        tracing::warn!("No search handler installed, using StubSearchHandler.");
        StubSearchHandler.search(req)
    }
}

fn to_hit<D: Document>(doc: &D, fields: &MetaFields, score: Score) -> Option<SearchHit> {
    let mut key = None;
    let mut name = None;
    let mut path = None;
    let mut ext = None;
    let mut size = None;
    let mut modified = None;

    for (field, value) in doc.iter_fields_and_values() {
        match field {
            f if f == fields.doc_key => {
                if let Some(v) = value.as_u64() {
                    key = Some(core_types::DocKey(v));
                }
            }
            f if f == fields.name => {
                if let Some(s) = value.as_str() {
                    name = Some(s.to_string());
                }
            }
            f if f == fields.path => {
                if let Some(s) = value.as_str() {
                    path = Some(s.to_string());
                }
            }
            f if f == fields.ext => {
                if let Some(s) = value.as_str() {
                    ext = Some(s.to_string());
                }
            }
            f if f == fields.size => {
                if let Some(v) = value.as_u64() {
                    size = Some(v);
                }
            }
            f if f == fields.modified => {
                if let Some(v) = value.as_i64() {
                    modified = Some(v);
                }
            }
            _ => {}
        }
    }

    key.map(|doc_key| SearchHit {
        key: doc_key,
        score,
        name,
        path,
        ext,
        size,
        modified,
        snippet: None,
    })
}
