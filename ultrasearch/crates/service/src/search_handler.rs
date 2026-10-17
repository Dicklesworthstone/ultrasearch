use crate::planner::QueryPlanner;
use crate::scanner::{Visibility, read_visibility};
use anyhow::{Context, Result, bail, ensure};
use content_index::{ContentIndex, open_or_create as open_content};
use core_types::DocKey;
use ipc::{
    FieldKind, QueryExpr, RangeExpr, RangeOp, RangeValue, SearchHit, SearchMode, SearchRequest,
    SearchResponse, TermExpr, TermModifier,
};
use meta_index::{MetaFields, MetaIndex, open_or_create_index, open_reader};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::ops::Bound;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;
use tantivy::collector::{Count, ScoreSegmentTweaker, ScoreTweaker, TopDocs};
use tantivy::columnar::StrColumn;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, QueryParser,
    QueryParserError, RangeQuery, TermQuery,
};
use tantivy::schema::{Document, Field, IndexRecordOption, TantivyDocument, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::{DocAddress, DocId, IndexReader, Score, Searcher, SegmentReader, Term};
use tracing::warn;

// Offset pagination keeps only bounded ranked candidates, never every matching
// document or its stored paths. Requests crossing either bound retain their
// exact total and explicitly report truncation; callers can narrow the query.
const MAX_SEARCH_WINDOW: usize = 10_000;
const MAX_SEARCH_PAGE: usize = 1_000;
const MAX_SNIPPET_CHARS: usize = 200;

struct SearchWindow {
    offset: usize,
    limit: usize,
    candidates: usize,
    truncated: bool,
}

impl SearchWindow {
    fn for_request(req: &SearchRequest) -> Self {
        let offset = req.offset as usize;
        let requested = req.limit.max(1) as usize;
        let limit = requested
            .min(MAX_SEARCH_PAGE)
            .min(MAX_SEARCH_WINDOW.saturating_sub(offset));
        Self {
            offset,
            limit,
            candidates: if limit == 0 { 0 } else { offset + limit },
            truncated: limit < requested,
        }
    }
}

type StableScore = (Score, Reverse<DocKey>);
type RankedDocument = (StableScore, DocAddress);

struct StableScoring {
    invalid: Arc<AtomicBool>,
}

struct StableSegmentScoring {
    keys: StrColumn,
    encoded: String,
    invalid: Arc<AtomicBool>,
}

impl ScoreTweaker<StableScore> for StableScoring {
    type Child = StableSegmentScoring;

    fn segment_tweaker(&self, reader: &SegmentReader) -> tantivy::Result<Self::Child> {
        let keys = reader.fast_fields().str("doc_key")?.ok_or_else(|| {
            tantivy::TantivyError::SchemaError("document identity fast field is missing".into())
        })?;
        Ok(StableSegmentScoring {
            keys,
            encoded: String::new(),
            invalid: Arc::clone(&self.invalid),
        })
    }
}

impl ScoreSegmentTweaker<StableScore> for StableSegmentScoring {
    fn score(&mut self, doc: DocId, score: Score) -> StableScore {
        let mut ordinals = self.keys.term_ords(doc);
        let key = ordinals.next().and_then(|ordinal| {
            if ordinals.next().is_some()
                || !self.keys.ord_to_str(ordinal, &mut self.encoded).ok()?
            {
                return None;
            }
            self.encoded.parse::<DocKey>().ok()
        });
        if let Some(key) = key
            && score.is_finite()
        {
            // PartialOrd in Tantivy's tuple collector treats both signed zero
            // scores as equal. Canonicalize them so the final total_cmp uses
            // exactly the same order at pagination boundaries.
            let score = if score == 0.0 { 0.0 } else { score };
            (score, Reverse(key))
        } else {
            // Tantivy's per-document score callback cannot return an error.
            // Retain failure across segment collectors and reject the complete
            // response afterward instead of returning a corrupt partial page.
            self.invalid.store(true, Ordering::Relaxed);
            (Score::NEG_INFINITY, Reverse(DocKey(u128::MAX)))
        }
    }
}

fn ranked_documents(
    searcher: &Searcher,
    query: &dyn Query,
    limit: usize,
) -> Result<(Vec<RankedDocument>, usize)> {
    if limit == 0 {
        return Ok((Vec::new(), searcher.search(query, &Count)?));
    }
    let invalid = Arc::new(AtomicBool::new(false));
    let collector = TopDocs::with_limit(limit).tweak_score(StableScoring {
        invalid: Arc::clone(&invalid),
    });
    let found = searcher.search(query, &(collector, Count))?;
    ensure!(
        !invalid.load(Ordering::Relaxed),
        "search encountered an invalid document identity or score"
    );
    Ok(found)
}

struct RankedHit {
    key: DocKey,
    score: Score,
    meta_address: Option<DocAddress>,
    content_address: Option<DocAddress>,
}

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
                // Hybrid union counting runs the already-parsed metadata
                // predicate on content rows. The shared fields must have the
                // same IDs, types, indexing options, and analyzers; parsing the
                // text again against the larger schema could change meaning.
                let content_schema = idx.index.schema();
                for (field, definition) in meta.index.schema().fields() {
                    let content_field = content_schema.get_field(definition.name())?;
                    ensure!(
                        field == content_field
                            && definition == content_schema.get_field_entry(content_field),
                        "content metadata fields cannot preserve the metadata query predicate"
                    );
                }
                Some((idx, reader))
            }
            Err(e) => {
                return Err(e.context(
                    "content index unavailable; ingestion cannot be reported as healthy",
                ));
            }
        };

        Ok(Self {
            meta,
            meta_reader,
            content,
        })
    }

    #[allow(dead_code)]
    fn build_meta_query(&self, expr: &QueryExpr) -> Result<Box<dyn Query>> {
        let optimized = QueryPlanner::optimize(expr.clone());
        Ok(self
            .build_meta_query_opt(&optimized)?
            .unwrap_or_else(|| Box::new(EmptyQuery)))
    }

    fn build_meta_query_opt(&self, expr: &QueryExpr) -> Result<Option<Box<dyn Query>>> {
        let fields = &self.meta.fields;
        let index = &self.meta.index;
        Ok(match expr {
            QueryExpr::Term(t) => self.term_query_meta(t, fields, index, false)?,
            QueryExpr::Range(range) => Some(range_query(range, index)?),
            QueryExpr::Not(inner) => match inner.as_ref() {
                QueryExpr::Term(t) => self
                    .term_query_meta(t, fields, index, true)?
                    .map(complement_query),
                QueryExpr::Range(range) => Some(complement_query(range_query(range, index)?)),
                other => self.build_meta_query_opt(other)?.map(complement_query),
            },
            QueryExpr::And(items) => {
                if items.is_empty() {
                    return Ok(None);
                }
                let mut clauses = Vec::with_capacity(items.len() + 1);
                let mut has_positive = false;
                let mut unrepresentable = false;
                for item in items {
                    if let QueryExpr::Not(neg_inner) = item {
                        let sub = match neg_inner.as_ref() {
                            QueryExpr::Term(t) => self.term_query_meta(t, fields, index, true)?,
                            QueryExpr::Range(range) => Some(range_query(range, index)?),
                            other => self.build_meta_query_opt(other)?,
                        };
                        if let Some(q) = sub {
                            clauses.push((Occur::MustNot, q));
                        } else {
                            unrepresentable = true;
                        }
                    } else if let Some(q) = self.build_meta_query_opt(item)? {
                        clauses.push((Occur::Must, q));
                        has_positive = true;
                    } else {
                        unrepresentable = true;
                    }
                }
                if unrepresentable {
                    return Ok(None);
                }
                if !has_positive {
                    clauses.insert(0, (Occur::Must, Box::new(AllQuery)));
                }
                Some(Box::new(BooleanQuery::new(clauses)))
            }
            QueryExpr::Or(items) => {
                if items.is_empty() {
                    return Ok(None);
                }
                let mut clauses = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(q) = self.build_meta_query_opt(item)? {
                        clauses.push((Occur::Should, q));
                    }
                }
                if clauses.is_empty() {
                    None
                } else {
                    Some(Box::new(BooleanQuery::new(clauses)))
                }
            }
        })
    }

    fn validate_raw_against_content_schema(&self, value: &str) -> Result<()> {
        if let Some((content_idx, _)) = &self.content {
            let default_fields = vec![
                content_idx.fields.name,
                content_idx.fields.path,
                content_idx.fields.content,
            ];
            let mut parser = QueryParser::for_index(&content_idx.index, default_fields);
            parser.set_conjunction_by_default();
            parse_raw_query_allowing_pure_negation(&parser, value)?;
        }
        Ok(())
    }

    fn term_query_meta(
        &self,
        term: &TermExpr,
        fields: &MetaFields,
        index: &tantivy::Index,
        negated: bool,
    ) -> Result<Option<Box<dyn Query>>> {
        validate_term_modifier(term.modifier)?;
        let value = term.value.trim();
        if value.is_empty() {
            return Ok(None);
        }

        match term.field {
            Some(
                FieldKind::Size
                | FieldKind::Modified
                | FieldKind::Created
                | FieldKind::Flags
                | FieldKind::Volume,
            ) => {
                bail!("numeric metadata fields require a range query");
            }
            Some(FieldKind::Content) => {
                if term.modifier == TermModifier::Term {
                    self.validate_raw_against_content_schema(value)?;
                }
                return Ok(None);
            }
            Some(FieldKind::Kind) => {
                return Ok(Some(kind_query(index, fields.ext, value, term.modifier)?));
            }
            None if negated => {
                // Negating an unfielded term requires verifying `NOT content:...`,
                // which the metadata index cannot evaluate on its own.
                if term.modifier == TermModifier::Term {
                    self.validate_raw_against_content_schema(value)?;
                }
                return Ok(None);
            }
            _ => {}
        }

        let target_fields: Vec<FieldKind> = match term.field {
            Some(f) => vec![f],
            None => vec![FieldKind::Name, FieldKind::Path],
        };

        let mut clauses = Vec::new();
        for field in target_fields {
            match field {
                FieldKind::Ext => {
                    let query = if term.modifier == TermModifier::Term {
                        Box::new(TermQuery::new(
                            Term::from_field_text(fields.ext, value),
                            IndexRecordOption::WithFreqs,
                        )) as Box<dyn Query>
                    } else {
                        modified_text_query(index, fields.ext, value, term.modifier)?
                    };
                    clauses.push((Occur::Should, query));
                }
                FieldKind::Name | FieldKind::Path => match term.modifier {
                    TermModifier::Prefix | TermModifier::Phrase | TermModifier::Fuzzy(_) => {
                        let pf = if matches!(field, FieldKind::Name) {
                            fields.name
                        } else {
                            fields.path
                        };
                        clauses.push((
                            Occur::Should,
                            modified_text_query(index, pf, value, term.modifier)?,
                        ));
                    }
                    TermModifier::Term => {
                        let pf = if matches!(field, FieldKind::Name) {
                            fields.name
                        } else {
                            fields.path
                        };
                        let mut parser = QueryParser::for_index(index, vec![pf]);
                        parser.set_conjunction_by_default();
                        match parse_raw_query_allowing_pure_negation(&parser, value) {
                            Ok((q, pure_neg)) => {
                                if (pure_neg || has_must_not_clause(q.as_ref()))
                                    && term.field.is_none()
                                {
                                    // Unfielded raw negation (`-needle` / `NOT needle`) requires
                                    // checking `content`, which the metadata index lacks.
                                    let mut explicit_only = QueryParser::for_index(index, vec![]);
                                    explicit_only.set_conjunction_by_default();
                                    if matches!(
                                        parse_raw_query_allowing_pure_negation(
                                            &explicit_only,
                                            value
                                        ),
                                        Err(QueryParserError::NoDefaultFieldDeclared)
                                    ) {
                                        self.validate_raw_against_content_schema(value)?;
                                        return Ok(None);
                                    }
                                }
                                let occur = if pure_neg && term.field.is_none() {
                                    Occur::Must
                                } else {
                                    Occur::Should
                                };
                                clauses.push((occur, q));
                            }
                            Err(QueryParserError::FieldDoesNotExist(missing))
                                if missing == "content" || missing == "content_lang" =>
                            {
                                self.validate_raw_against_content_schema(value)?;
                                return Ok(None);
                            }
                            Err(err) => return Err(err.into()),
                        }
                    }
                },
                _ => {}
            }
        }

        if clauses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Box::new(BooleanQuery::new(clauses))))
        }
    }

    fn build_content_query_opt(&self, expr: &QueryExpr) -> Result<Option<Box<dyn Query>>> {
        let (idx, _) = self
            .content
            .as_ref()
            .context("content index not available")?;
        let fields = &idx.fields;
        let index = &idx.index;

        Ok(match expr {
            QueryExpr::Term(t) => self.term_query_content(t, fields, index, false)?,
            QueryExpr::Range(range) => Some(range_query(range, index)?),
            QueryExpr::Not(inner) => match inner.as_ref() {
                QueryExpr::Term(t) => self
                    .term_query_content(t, fields, index, true)?
                    .map(complement_query),
                QueryExpr::Range(range) => Some(complement_query(range_query(range, index)?)),
                other => self.build_content_query_opt(other)?.map(complement_query),
            },
            QueryExpr::And(items) => {
                if items.is_empty() {
                    return Ok(None);
                }
                let mut clauses = Vec::with_capacity(items.len() + 1);
                let mut has_positive = false;
                let mut unrepresentable = false;
                for item in items {
                    if let QueryExpr::Not(neg_inner) = item {
                        let sub = match neg_inner.as_ref() {
                            QueryExpr::Term(t) => {
                                self.term_query_content(t, fields, index, true)?
                            }
                            QueryExpr::Range(range) => Some(range_query(range, index)?),
                            other => self.build_content_query_opt(other)?,
                        };
                        if let Some(q) = sub {
                            clauses.push((Occur::MustNot, q));
                        } else {
                            unrepresentable = true;
                        }
                    } else if let Some(q) = self.build_content_query_opt(item)? {
                        clauses.push((Occur::Must, q));
                        has_positive = true;
                    } else {
                        unrepresentable = true;
                    }
                }
                if unrepresentable {
                    return Ok(None);
                }
                if !has_positive {
                    clauses.insert(0, (Occur::Must, Box::new(AllQuery)));
                }
                Some(Box::new(BooleanQuery::new(clauses)))
            }
            QueryExpr::Or(items) => {
                if items.is_empty() {
                    return Ok(None);
                }
                let mut clauses = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(q) = self.build_content_query_opt(item)? {
                        clauses.push((Occur::Should, q));
                    }
                }
                if clauses.is_empty() {
                    None
                } else {
                    Some(Box::new(BooleanQuery::new(clauses)))
                }
            }
        })
    }

    fn term_query_content(
        &self,
        term: &TermExpr,
        fields: &content_index::ContentFields,
        index: &tantivy::Index,
        negated: bool,
    ) -> Result<Option<Box<dyn Query>>> {
        validate_term_modifier(term.modifier)?;
        let value = term.value.trim();
        if value.is_empty() {
            return Ok(None);
        }

        match term.field {
            Some(
                FieldKind::Size
                | FieldKind::Modified
                | FieldKind::Created
                | FieldKind::Flags
                | FieldKind::Volume,
            ) => {
                bail!("numeric metadata fields require a range query");
            }
            Some(FieldKind::Kind) => {
                return Ok(Some(kind_query(index, fields.ext, value, term.modifier)?));
            }
            _ => {}
        }

        let target_fields: Vec<FieldKind> = match term.field {
            Some(f) => vec![f],
            None if negated => vec![FieldKind::Name, FieldKind::Path, FieldKind::Content],
            None => vec![FieldKind::Name, FieldKind::Content],
        };

        let mut clauses = Vec::new();
        for field in target_fields {
            if matches!(field, FieldKind::Ext) {
                let query = if term.modifier == TermModifier::Term {
                    Box::new(TermQuery::new(
                        Term::from_field_text(fields.ext, value),
                        IndexRecordOption::WithFreqs,
                    )) as Box<dyn Query>
                } else {
                    modified_text_query(index, fields.ext, value, term.modifier)?
                };
                clauses.push((Occur::Should, query));
                continue;
            }

            let t_field = match field {
                FieldKind::Name => Some(fields.name),
                FieldKind::Path => Some(fields.path),
                FieldKind::Content => Some(fields.content),
                _ => None,
            };

            if let Some(tf) = t_field {
                match term.modifier {
                    TermModifier::Prefix | TermModifier::Phrase | TermModifier::Fuzzy(_) => {
                        clauses.push((
                            Occur::Should,
                            modified_text_query(index, tf, value, term.modifier)?,
                        ));
                    }
                    TermModifier::Term => {
                        if term.field.is_none() {
                            let default_fields = vec![fields.name, fields.path, fields.content];
                            let mut multi_parser = QueryParser::for_index(index, default_fields);
                            multi_parser.set_conjunction_by_default();
                            let (mq, pure_neg) =
                                parse_raw_query_allowing_pure_negation(&multi_parser, value)?;
                            if pure_neg || has_must_not_clause(mq.as_ref()) {
                                return Ok(Some(mq));
                            }
                        }
                        let mut parser = QueryParser::for_index(index, vec![tf]);
                        parser.set_conjunction_by_default();
                        let (q, _) = parse_raw_query_allowing_pure_negation(&parser, value)?;
                        clauses.push((Occur::Should, q));
                    }
                }
            }
        }
        if clauses.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Box::new(BooleanQuery::new(clauses))))
        }
    }

    fn search_visible(
        &self,
        req: &SearchRequest,
        visibility: &Visibility,
    ) -> Result<SearchResponse> {
        let start = Instant::now();
        let window = SearchWindow::for_request(req);
        let optimized_expr = QueryPlanner::optimize(req.query.clone());
        // Freeze each reader once for ranking, counting, and stored-field reads.
        // The caller holds the visibility guard through this whole operation.
        let meta_searcher = self.meta_reader.searcher();
        let content_searcher = self.content.as_ref().map(|(_, reader)| reader.searcher());
        let mut ranked = HashMap::<DocKey, RankedHit>::new();
        let mut total = 0u64;

        let meta_plan = if req.mode != SearchMode::Content {
            let query = self
                .build_meta_query_opt(&optimized_expr)?
                .unwrap_or_else(|| Box::new(EmptyQuery));
            let predicate = query.box_clone();
            let visible = visible_query(
                query,
                self.meta.fields.doc_key,
                self.meta.fields.volume,
                self.meta.fields.path_exact,
                visibility,
            )?;
            Some((visible, predicate))
        } else {
            None
        };

        let content_plan = if req.mode != SearchMode::NameOnly {
            let (content, _) = self
                .content
                .as_ref()
                .context("content index is unavailable")?;
            let query = self
                .build_content_query_opt(&optimized_expr)?
                .unwrap_or_else(|| Box::new(EmptyQuery));
            let snippet_query = query.box_clone();
            let additions = meta_plan
                .as_ref()
                .map(|(_, metadata)| {
                    let overlap = Box::new(BooleanQuery::new(vec![
                        (Occur::Must, query.box_clone()),
                        (Occur::MustNot, metadata.box_clone()),
                    ])) as Box<dyn Query>;
                    visible_query(
                        overlap,
                        content.fields.doc_key,
                        content.fields.volume,
                        content.fields.path_exact,
                        visibility,
                    )
                })
                .transpose()?;
            let visible = visible_query(
                query,
                content.fields.doc_key,
                content.fields.volume,
                content.fields.path_exact,
                visibility,
            )?;
            Some((visible, additions, snippet_query))
        } else {
            None
        };

        let (meta_outcome, content_outcome) = std::thread::scope(|scope| -> Result<_> {
            let meta_handle = meta_plan.as_ref().map(|(visible, _)| {
                let searcher = &meta_searcher;
                let candidates = window.candidates;
                scope.spawn(move || ranked_documents(searcher, visible.as_ref(), candidates))
            });

            let content_res: Result<_> = if let Some((visible, additions, _)) = &content_plan {
                let searcher = content_searcher
                    .as_ref()
                    .context("content reader is unavailable")?;
                let (documents, count) =
                    ranked_documents(searcher, visible.as_ref(), window.candidates)?;
                let added_count = if let Some(additions) = additions {
                    // Published content rows have the same metadata as their one
                    // same-key metadata row: apply_metadata copies the committed
                    // worker snapshot or removes both rows, and pending mutations
                    // are masked. Shared field IDs/options were verified on open.
                    // Thus |M union C| = |M| + |C and not M| without collecting an
                    // unbounded set of document identities. Scores do not affect
                    // membership in this cloned metadata predicate.
                    searcher.search(additions.as_ref(), &Count)? as u64
                } else {
                    count as u64
                };
                Ok(Some((documents, added_count)))
            } else {
                Ok(None)
            };

            let meta_res =
                match meta_handle {
                    Some(handle) => Some(handle.join().map_err(|_| {
                        anyhow::anyhow!("metadata search worker thread panicked")
                    })??),
                    None => None,
                };

            Ok::<_, anyhow::Error>((meta_res, content_res?))
        })?;

        if let Some((documents, count)) = meta_outcome {
            total = count as u64;
            for ((score, Reverse(key)), address) in documents {
                ranked.insert(
                    key,
                    RankedHit {
                        key,
                        score,
                        meta_address: Some(address),
                        content_address: None,
                    },
                );
            }
        }

        if let Some((documents, added_count)) = content_outcome {
            if meta_plan.is_some() {
                total += added_count;
            } else {
                total = added_count;
            }
            for ((score, Reverse(key)), address) in documents {
                ranked
                    .entry(key)
                    .and_modify(|hit| {
                        hit.score = hit.score.max(score);
                        hit.content_address = Some(address);
                    })
                    .or_insert(RankedHit {
                        key,
                        score,
                        meta_address: None,
                        content_address: Some(address),
                    });
            }
        }

        let snippet_generator =
            if let (Some((content, _)), Some(searcher), Some((_, _, snippet_query))) =
                (&self.content, &content_searcher, &content_plan)
            {
                let mut snippet_gen = SnippetGenerator::create(
                    searcher,
                    snippet_query.as_ref(),
                    content.fields.content,
                )
                .ok();
                if let Some(ref mut g) = snippet_gen {
                    g.set_max_num_chars(MAX_SNIPPET_CHARS);
                }
                snippet_gen
            } else {
                None
            };

        // With max-score merging, a union top-k hit must occur in at least one
        // source's top-k: that source's k preceding distinct keys would
        // otherwise precede it in the union too. Both source collectors use the
        // identical full-key tie-break, including at the candidate boundary.
        let mut ranked: Vec<_> = ranked.into_values().collect();
        ranked.sort_unstable_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.key.cmp(&right.key))
        });
        let hits = ranked
            .into_iter()
            .skip(window.offset)
            .take(window.limit)
            .map(|hit| {
                let mut loaded = if let Some(address) = hit.meta_address {
                    to_hit(
                        &meta_searcher.doc::<TantivyDocument>(address)?,
                        &self.meta.fields,
                        hit.score,
                    )
                    .context("search result has no valid stored identity")?
                } else if let Some(address) = hit.content_address {
                    let (content, _) = self
                        .content
                        .as_ref()
                        .context("content index is unavailable")?;
                    let searcher = content_searcher
                        .as_ref()
                        .context("content reader is unavailable")?;
                    to_hit_content(
                        &searcher.doc::<TantivyDocument>(address)?,
                        &content.fields,
                        hit.score,
                        snippet_generator.as_ref(),
                    )
                    .context("search result has no valid stored identity")?
                } else {
                    bail!("ranked hit has neither metadata nor content address");
                };

                if loaded.snippet.is_none()
                    && let Some(content_address) = hit.content_address
                    && let (Some((content, _)), Some(searcher)) = (&self.content, &content_searcher)
                {
                    let content_doc = searcher.doc::<TantivyDocument>(content_address)?;
                    loaded.snippet = extract_content_snippet(
                        &content_doc,
                        &content.fields,
                        snippet_generator.as_ref(),
                    );
                }

                ensure!(
                    loaded.key == hit.key,
                    "stored and ranked document identities differ"
                );
                Ok(loaded)
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(SearchResponse {
            id: req.id,
            hits,
            total,
            truncated: window.truncated,
            took_ms: start.elapsed().as_millis().min(u32::MAX as u128) as u32,
            served_by: None,
        })
    }
}

impl SearchHandler for UnifiedSearchHandler {
    fn search(&self, req: SearchRequest) -> SearchResponse {
        // Hold the visibility read lock through both reads. Publishing an intent
        // and unmasking a committed batch serialize against complete queries.
        let visibility = read_visibility();
        if let Err(error) = self.meta_reader.reload().and_then(|()| {
            if let Some((_, reader)) = &self.content {
                reader.reload()
            } else {
                Ok(())
            }
        }) {
            crate::status_provider::update_status_ingestion_state(format!(
                "search reader reload failed: {error}"
            ));
            let mut response = StubSearchHandler.search(req);
            response.served_by = Some("service-unavailable".into());
            response.truncated = true;
            return response;
        }
        match self.search_visible(&req, &visibility) {
            Ok(response) => response,
            Err(error) => {
                warn!(%error, "search failed; refusing a partial or healthy-empty response");
                let mut response = StubSearchHandler.search(req);
                response.served_by = Some("service-unavailable".into());
                response.truncated = true;
                response
            }
        }
    }
}

fn complement_query(inner: Box<dyn Query>) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(vec![
        (Occur::Must, Box::new(AllQuery)),
        (Occur::MustNot, inner),
    ]))
}

fn has_must_not_clause(query: &dyn Query) -> bool {
    query
        .as_any()
        .downcast_ref::<BooleanQuery>()
        .is_some_and(|bq| {
            bq.clauses()
                .iter()
                .any(|(occur, _)| *occur == Occur::MustNot)
        })
}

fn parse_raw_query_allowing_pure_negation(
    parser: &QueryParser,
    value: &str,
) -> std::result::Result<(Box<dyn Query>, bool), QueryParserError> {
    match parser.parse_query(value) {
        Ok(q) => Ok((normalize_raw_boolean_query(q), false)),
        Err(QueryParserError::AllButQueryForbidden) => {
            let rewritten = format!("* {value}");
            match parser.parse_query(&rewritten) {
                Ok(q) => Ok((normalize_raw_boolean_query(q), true)),
                Err(_) => {
                    let rewritten_and = format!("* AND ({value})");
                    let q = parser.parse_query(&rewritten_and)?;
                    Ok((normalize_raw_boolean_query(q), true))
                }
            }
        }
        Err(e) => Err(e),
    }
}

fn normalize_raw_boolean_query(query: Box<dyn Query>) -> Box<dyn Query> {
    if let Some(bq) = query.as_any().downcast_ref::<BooleanQuery>()
        && !bq.clauses().is_empty()
        && bq
            .clauses()
            .iter()
            .all(|(occur, _)| *occur == Occur::MustNot)
    {
        let mut clauses = Vec::with_capacity(bq.clauses().len() + 1);
        clauses.push((Occur::Must, Box::new(AllQuery) as Box<dyn Query>));
        for (occur, sub) in bq.clauses() {
            clauses.push((*occur, sub.box_clone()));
        }
        return Box::new(BooleanQuery::new(clauses));
    }
    query
}

fn kind_extensions(kind: &str) -> Result<&'static [&'static str]> {
    match kind.trim().to_ascii_lowercase().as_str() {
        "docs" | "doc" | "document" | "documents" => Ok(&[
            "pdf", "doc", "docx", "txt", "md", "rtf", "odt", "pages", "tex", "epub", "xls", "xlsx",
            "ppt", "pptx", "csv",
        ]),
        "code" | "src" | "source" => Ok(&[
            "rs", "py", "js", "ts", "tsx", "jsx", "c", "cpp", "h", "hpp", "cs", "go", "java", "kt",
            "swift", "rb", "php", "sh", "ps1", "sql", "html", "css", "scss", "toml", "yaml", "yml",
            "json", "xml",
        ]),
        "image" | "images" | "img" | "photo" | "photos" => Ok(&[
            "jpg", "jpeg", "png", "gif", "bmp", "svg", "webp", "ico", "tiff", "tif", "heic", "raw",
        ]),
        "video" | "videos" | "movie" | "movies" => Ok(&[
            "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpeg", "mpg",
        ]),
        "audio" | "music" | "sound" => {
            Ok(&["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus"])
        }
        "archive" | "archives" | "zip" | "compressed" => Ok(&[
            "zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso",
        ]),
        "data" | "dataset" | "db" | "database" => Ok(&[
            "json", "jsonl", "csv", "tsv", "parquet", "db", "sqlite", "sqlite3", "xml", "yaml",
            "yml", "toml",
        ]),
        "exec" | "executable" | "bin" | "app" => Ok(&[
            "exe", "msi", "bat", "cmd", "ps1", "com", "dll", "sys", "app", "sh",
        ]),
        other => bail!("unsupported file kind: {other}"),
    }
}

fn kind_query(
    index: &tantivy::Index,
    ext_field: Field,
    value: &str,
    modifier: TermModifier,
) -> Result<Box<dyn Query>> {
    validate_term_modifier(modifier)?;
    let exts = kind_extensions(value)?;
    let mut clauses = Vec::with_capacity(exts.len());
    for &ext in exts {
        let q = if modifier == TermModifier::Term {
            Box::new(TermQuery::new(
                Term::from_field_text(ext_field, ext),
                IndexRecordOption::WithFreqs,
            )) as Box<dyn Query>
        } else {
            modified_text_query(index, ext_field, ext, modifier)?
        };
        clauses.push((Occur::Should, q));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn range_query(range: &RangeExpr, index: &tantivy::Index) -> Result<Box<dyn Query>> {
    let name = match range.field {
        FieldKind::Size => "size",
        FieldKind::Volume => "volume",
        FieldKind::Flags => "flags",
        FieldKind::Created => "created",
        FieldKind::Modified => "modified",
        _ => bail!("range queries require a numeric metadata field"),
    };
    let field = index.schema().get_field(name)?;
    // Fast-field range execution requires the exact schema type. In particular,
    // never cast unsigned sizes through i64 or pass a mismatched IPC value type
    // to Tantivy, whose fast-field weight asserts that these types agree.
    let query = match range.value {
        RangeValue::U64 { lo, hi } => {
            ensure!(
                matches!(
                    range.field,
                    FieldKind::Size | FieldKind::Volume | FieldKind::Flags
                ),
                "{name} range values must use signed integers"
            );
            numeric_range(range.op, lo, hi, |value| Term::from_field_u64(field, value))?
        }
        RangeValue::I64 { lo, hi } => {
            ensure!(
                matches!(range.field, FieldKind::Created | FieldKind::Modified),
                "{name} range values must use unsigned integers"
            );
            numeric_range(range.op, lo, hi, |value| Term::from_field_i64(field, value))?
        }
    };
    Ok(Box::new(query))
}

fn numeric_range<T: Copy + Ord>(
    op: RangeOp,
    lo: T,
    hi: Option<T>,
    term: impl Fn(T) -> Term,
) -> Result<RangeQuery> {
    if op != RangeOp::Between {
        ensure!(hi.is_none(), "only Between accepts a second range bound");
    }
    let (lower, upper) = match op {
        RangeOp::Gt => (Bound::Excluded(term(lo)), Bound::Unbounded),
        RangeOp::Ge => (Bound::Included(term(lo)), Bound::Unbounded),
        RangeOp::Lt => (Bound::Unbounded, Bound::Excluded(term(lo))),
        RangeOp::Le => (Bound::Unbounded, Bound::Included(term(lo))),
        RangeOp::Between => {
            let hi = hi.context("Between requires both inclusive range bounds")?;
            ensure!(lo <= hi, "range lower bound exceeds its upper bound");
            (Bound::Included(term(lo)), Bound::Included(term(hi)))
        }
    };
    // Bound preserves strict comparisons at integer extrema without adding or
    // subtracting one in service code. At least one bound is always present.
    Ok(RangeQuery::new(lower, upper))
}

fn validate_term_modifier(modifier: TermModifier) -> Result<()> {
    if let TermModifier::Fuzzy(distance) = modifier {
        ensure!(distance <= 2, "fuzzy distance must be between zero and two");
    }
    Ok(())
}

fn modified_text_query(
    index: &tantivy::Index,
    field: Field,
    value: &str,
    modifier: TermModifier,
) -> Result<Box<dyn Query>> {
    validate_term_modifier(modifier)?;
    let mut analyzer = index.tokenizer_for_field(field)?;
    let mut stream = analyzer.token_stream(value);
    let mut terms = Vec::new();
    stream.process(&mut |token| {
        terms.push((token.position, Term::from_field_text(field, &token.text)));
    });
    if terms.is_empty() {
        return Ok(Box::new(EmptyQuery));
    }
    if modifier == TermModifier::Phrase {
        return if terms.len() == 1 {
            Ok(Box::new(TermQuery::new(
                terms.pop().context("phrase has no analyzed term")?.1,
                IndexRecordOption::WithFreqs,
            )))
        } else {
            Ok(Box::new(PhraseQuery::new_with_offset(terms)))
        };
    }
    // Literal modifiers use the field's indexing analyzer rather than parsing
    // their value as query syntax. Multiple analyzed tokens are conjunctive;
    // Phrase additionally requires their indexed positions. Prefix is a true
    // zero-edit prefix automaton, without a lossy cap on expanded dictionary
    // terms. Fuzzy uses the supported edit bound, counting transpositions once.
    let mut clauses = Vec::with_capacity(terms.len());
    for (_, term) in terms {
        let query = match modifier {
            TermModifier::Prefix => FuzzyTermQuery::new_prefix(term, 0, false),
            TermModifier::Fuzzy(distance) => FuzzyTermQuery::new(term, distance, true),
            TermModifier::Term | TermModifier::Phrase => {
                bail!("literal text query requires Prefix, Phrase, or Fuzzy")
            }
        };
        clauses.push((Occur::Must, Box::new(query) as Box<dyn Query>));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn visible_query(
    query: Box<dyn Query>,
    key_field: tantivy::schema::Field,
    volume_field: tantivy::schema::Field,
    path_field: tantivy::schema::Field,
    visibility: &Visibility,
) -> Result<Box<dyn Query>> {
    if visibility.documents.is_empty()
        && visibility.volumes.is_empty()
        && visibility.directories.is_empty()
    {
        return Ok(query);
    }
    let mut clauses = vec![(Occur::Must, query)];
    for key in &visibility.documents {
        clauses.push((
            Occur::MustNot,
            Box::new(TermQuery::new(
                Term::from_field_text(key_field, &key.to_string()),
                IndexRecordOption::Basic,
            )),
        ));
    }
    for volume in &visibility.volumes {
        clauses.push((
            Occur::MustNot,
            Box::new(TermQuery::new(
                Term::from_field_u64(volume_field, u64::from(*volume)),
                IndexRecordOption::Basic,
            )),
        ));
    }
    for (volume, paths) in &visibility.directories {
        let subtree = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(volume_field, u64::from(*volume)),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                meta_index::path_prefix_query(path_field, paths)?,
            ),
        ]);
        clauses.push((Occur::MustNot, Box::new(subtree)));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn truncate_snippet_chars(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut end_byte = trimmed.len();
    for (count, (idx, _)) in trimmed.char_indices().enumerate() {
        if count == max_chars {
            end_byte = idx;
            break;
        }
    }
    if end_byte < trimmed.len() {
        format!("{}…", trimmed[..end_byte].trim_end())
    } else {
        trimmed.to_string()
    }
}

fn extract_content_snippet(
    doc: &TantivyDocument,
    fields: &content_index::ContentFields,
    snippet_generator: Option<&SnippetGenerator>,
) -> Option<String> {
    if let Some(generator) = snippet_generator {
        let snippet = generator.snippet_from_doc(doc);
        let fragment = snippet.fragment().trim();
        if !fragment.is_empty() {
            return Some(truncate_snippet_chars(fragment, MAX_SNIPPET_CHARS));
        }
    }
    let raw = doc
        .get_first(fields.content)
        .and_then(|v| v.as_str())?
        .trim();
    if raw.is_empty() {
        None
    } else {
        Some(truncate_snippet_chars(raw, MAX_SNIPPET_CHARS))
    }
}

// Helper to map content doc to SearchHit
fn to_hit_content(
    doc: &TantivyDocument,
    fields: &content_index::ContentFields,
    score: Score,
    snippet_generator: Option<&SnippetGenerator>,
) -> Option<SearchHit> {
    let mut key = None;
    let mut name = None;
    let mut path = None;
    let mut ext = None;
    let mut size = None;
    let mut modified = None;

    for (field, value) in doc.iter_fields_and_values() {
        match field {
            f if f == fields.doc_key => {
                key = value.as_str().and_then(|text| text.parse().ok());
            }
            f if f == fields.name => name = value.as_str().map(|s| s.to_string()),
            f if f == fields.path => path = value.as_str().map(|s| s.to_string()),
            f if f == fields.ext => ext = value.as_str().map(|s| s.to_string()),
            f if f == fields.size => size = value.as_u64(),
            f if f == fields.modified => modified = value.as_i64(),
            _ => {}
        }
    }

    let snippet = extract_content_snippet(doc, fields, snippet_generator);

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

static HANDLER: OnceLock<RwLock<Option<Box<dyn SearchHandler>>>> = OnceLock::new();

pub fn set_search_handler(handler: Box<dyn SearchHandler>) {
    tracing::info!("Global search handler installed.");
    *HANDLER
        .get_or_init(RwLock::default)
        .write()
        .unwrap_or_else(|e| e.into_inner()) = Some(handler);
}

pub fn search(req: SearchRequest) -> SearchResponse {
    tracing::info!("Received search request id={} mode={:?}", req.id, req.mode);
    let handler = HANDLER
        .get_or_init(RwLock::default)
        .read()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(h) = handler.as_ref() {
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
                key = value.as_str().and_then(|text| text.parse().ok());
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

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{FileFlags, FileMeta};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use tantivy::IndexWriter;
    use tantivy::merge_policy::NoMergePolicy;

    struct SearchFixture {
        root: PathBuf,
        handler: UnifiedSearchHandler,
        metadata: IndexWriter,
        content: IndexWriter,
    }

    impl SearchFixture {
        fn new() -> Result<Self> {
            let root = tempfile::tempdir()?.keep();
            let handler = UnifiedSearchHandler::try_new(&root.join("meta"), &root.join("content"))?;
            let metadata = meta_index::create_writer(
                &handler.meta,
                &meta_index::WriterConfig {
                    heap_size_bytes: 20_000_000,
                    num_threads: 1,
                },
            )?;
            let content = content_index::create_writer(
                &handler.content.as_ref().unwrap().0,
                &content_index::WriterConfig {
                    heap_size_bytes: 20_000_000,
                    num_threads: 1,
                },
            )?;
            metadata.set_merge_policy(Box::new(NoMergePolicy));
            content.set_merge_policy(Box::new(NoMergePolicy));
            Ok(Self {
                root,
                handler,
                metadata,
                content,
            })
        }

        fn put_metadata(&mut self, meta: &FileMeta) -> Result<()> {
            meta_index::add_file_meta_batch(
                &mut self.metadata,
                &self.handler.meta.fields,
                [meta.clone()],
            )
        }

        fn put_content(&mut self, meta: &FileMeta, body: Option<&str>) -> Result<()> {
            let fields = &self.handler.content.as_ref().unwrap().0.fields;
            if let Some(body) = body {
                content_index::add_content_doc(
                    &mut self.content,
                    fields,
                    &content_index::ContentDoc {
                        key: meta.key,
                        volume: meta.volume,
                        name: Some(meta.name.clone()),
                        path: meta.path.clone(),
                        ext: meta.ext.clone(),
                        size: meta.size,
                        created: meta.created,
                        modified: meta.modified,
                        flags: u64::from(meta.flags.bits()),
                        content_lang: None,
                        content: body.to_string(),
                    },
                )?;
            } else {
                content_index::delete_doc(&mut self.content, fields, meta.key);
            }
            Ok(())
        }

        fn put(&mut self, meta: &FileMeta, body: Option<&str>) -> Result<()> {
            self.put_metadata(meta)?;
            self.put_content(meta, body)
        }

        fn delete(&mut self, key: DocKey) {
            meta_index::delete_doc(&mut self.metadata, &self.handler.meta.fields, key);
            content_index::delete_doc(
                &mut self.content,
                &self.handler.content.as_ref().unwrap().0.fields,
                key,
            );
        }

        fn commit(&mut self) -> Result<()> {
            self.content.commit()?;
            self.metadata.commit()?;
            self.reload()
        }

        fn reload(&self) -> Result<()> {
            self.handler.meta_reader.reload()?;
            self.handler.content.as_ref().unwrap().1.reload()?;
            Ok(())
        }

        fn search(&self, req: &SearchRequest, visibility: &Visibility) -> Result<SearchResponse> {
            self.handler.search_visible(req, visibility)
        }
    }

    fn file(volume: u16, identity: u64, name: &str, path: &str) -> FileMeta {
        FileMeta {
            key: DocKey::from_parts(volume, identity),
            volume,
            parent: None,
            name: name.to_string(),
            path: Some(path.to_string()),
            ext: Some("txt".to_string()),
            size: 100,
            created: 100,
            modified: 200,
            flags: FileFlags::empty(),
        }
    }

    fn request(value: &str, field: Option<FieldKind>, mode: SearchMode) -> SearchRequest {
        SearchRequest::with_query(QueryExpr::Term(TermExpr {
            field,
            value: value.to_string(),
            modifier: TermModifier::Term,
        }))
        .with_mode(mode)
        .with_limit(200)
    }

    fn keys(response: &SearchResponse) -> Vec<DocKey> {
        response.hits.iter().map(|hit| hit.key).collect()
    }

    fn literal(field: FieldKind, value: &str, modifier: TermModifier) -> QueryExpr {
        QueryExpr::Term(TermExpr {
            field: Some(field),
            value: value.to_string(),
            modifier,
        })
    }

    fn range(field: FieldKind, op: RangeOp, value: RangeValue) -> QueryExpr {
        QueryExpr::Range(RangeExpr { field, op, value })
    }

    fn assert_query_members(
        fixture: &SearchFixture,
        query: &QueryExpr,
        metadata: &[DocKey],
        content: &[DocKey],
        visibility: &Visibility,
    ) -> Result<()> {
        for mode in [
            SearchMode::NameOnly,
            SearchMode::Content,
            SearchMode::Auto,
            SearchMode::Hybrid,
        ] {
            let expected: BTreeSet<_> = match mode {
                SearchMode::NameOnly => metadata.iter().copied().collect(),
                SearchMode::Content => content.iter().copied().collect(),
                SearchMode::Auto | SearchMode::Hybrid => {
                    metadata.iter().chain(content).copied().collect()
                }
            };
            let req = SearchRequest::with_query(query.clone())
                .with_mode(mode)
                .with_limit(200);
            let response = fixture.search(&req, visibility)?;
            let actual = keys(&response);
            assert_eq!(response.total, expected.len() as u64, "{mode:?}: {query:?}");
            assert_eq!(actual.len(), expected.len(), "{mode:?}: {query:?}");
            assert_eq!(
                actual.into_iter().collect::<BTreeSet<_>>(),
                expected,
                "{mode:?}: {query:?}"
            );
            assert!(!response.truncated);
            assert!(response.served_by.is_none());
        }
        Ok(())
    }

    #[test]
    fn unsigned_ranges_preserve_strict_bounds_and_full_size_domain() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut files = Vec::new();
        for (identity, size) in [0, 9, 10, 11, u64::MAX].into_iter().enumerate() {
            let mut meta = file(
                if identity % 2 == 0 { 2 } else { 10 },
                0xabcd_0000_0000_0000 | identity as u64,
                "range candidate",
                &format!(r"C:\ranges\{identity}.txt"),
            );
            meta.size = size;
            meta.flags = match identity {
                1 => FileFlags::HIDDEN,
                2 => FileFlags::IS_DIR,
                3 => FileFlags::SYSTEM,
                4 => FileFlags::HIDDEN | FileFlags::SYSTEM,
                _ => FileFlags::empty(),
            };
            fixture.put(&meta, (identity != 2).then_some("indexed body"))?;
            files.push(meta);
        }
        fixture.commit()?;
        let cases: &[(RangeOp, u64, Option<u64>, &[usize])] = &[
            (RangeOp::Gt, 10, None, &[3, 4]),
            (RangeOp::Ge, 10, None, &[2, 3, 4]),
            (RangeOp::Lt, 10, None, &[0, 1]),
            (RangeOp::Le, 10, None, &[0, 1, 2]),
            (RangeOp::Between, 9, Some(11), &[1, 2, 3]),
            (RangeOp::Between, 10, Some(10), &[2]),
            (RangeOp::Gt, u64::MAX, None, &[]),
            (RangeOp::Ge, u64::MAX, None, &[4]),
            (RangeOp::Gt, i64::MAX as u64, None, &[4]),
            (RangeOp::Lt, 0, None, &[]),
            (RangeOp::Le, 0, None, &[0]),
            (RangeOp::Between, 0, Some(u64::MAX), &[0, 1, 2, 3, 4]),
        ];
        for &(op, lo, hi, indices) in cases {
            let metadata: Vec<_> = indices.iter().map(|&i| files[i].key).collect();
            let content: Vec<_> = indices
                .iter()
                .filter(|&&i| i != 2)
                .map(|&i| files[i].key)
                .collect();
            assert_query_members(
                &fixture,
                &range(FieldKind::Size, op, RangeValue::U64 { lo, hi }),
                &metadata,
                &content,
                &Visibility::default(),
            )?;
        }
        for (field, op, value) in [
            (
                FieldKind::Volume,
                RangeOp::Ge,
                RangeValue::U64 { lo: 10, hi: None },
            ),
            (
                FieldKind::Flags,
                RangeOp::Between,
                RangeValue::U64 { lo: 2, hi: Some(4) },
            ),
        ] {
            let expected = [files[1].key, files[3].key];
            assert_query_members(
                &fixture,
                &range(field, op, value),
                &expected,
                &expected,
                &Visibility::default(),
            )?;
        }
        Ok(())
    }

    #[test]
    fn signed_timestamp_ranges_preserve_negative_values_and_extrema() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut files = Vec::new();
        for (identity, timestamp) in [i64::MIN, -1, 0, 1, i64::MAX].into_iter().enumerate() {
            let mut meta = file(2, identity as u64, "timestamp", r"C:\time\entry.txt");
            meta.created = timestamp;
            meta.modified = timestamp;
            fixture.put(&meta, Some("indexed body"))?;
            files.push(meta);
        }
        fixture.commit()?;
        let cases: &[(RangeOp, i64, Option<i64>, &[usize])] = &[
            (RangeOp::Gt, 0, None, &[3, 4]),
            (RangeOp::Ge, 0, None, &[2, 3, 4]),
            (RangeOp::Lt, 0, None, &[0, 1]),
            (RangeOp::Le, 0, None, &[0, 1, 2]),
            (RangeOp::Between, -1, Some(1), &[1, 2, 3]),
            (RangeOp::Between, -1, Some(-1), &[1]),
            (RangeOp::Gt, i64::MAX, None, &[]),
            (RangeOp::Ge, i64::MAX, None, &[4]),
            (RangeOp::Lt, i64::MIN, None, &[]),
            (RangeOp::Le, i64::MIN, None, &[0]),
            (RangeOp::Between, i64::MIN, Some(i64::MAX), &[0, 1, 2, 3, 4]),
        ];
        for field in [FieldKind::Created, FieldKind::Modified] {
            for &(op, lo, hi, indices) in cases {
                let expected: Vec<_> = indices.iter().map(|&i| files[i].key).collect();
                assert_query_members(
                    &fixture,
                    &range(field, op, RangeValue::I64 { lo, hi }),
                    &expected,
                    &expected,
                    &Visibility::default(),
                )?;
            }
        }
        Ok(())
    }

    #[test]
    fn malformed_typed_ranges_and_fuzzy_distances_reject_the_whole_request() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        fixture.put(&file(2, 1, "needle", r"C:\needle.txt"), Some("needle"))?;
        fixture.commit()?;
        let invalid = [
            range(
                FieldKind::Size,
                RangeOp::Ge,
                RangeValue::I64 { lo: -1, hi: None },
            ),
            range(
                FieldKind::Modified,
                RangeOp::Ge,
                RangeValue::U64 { lo: 0, hi: None },
            ),
            range(
                FieldKind::Name,
                RangeOp::Ge,
                RangeValue::U64 { lo: 0, hi: None },
            ),
            range(
                FieldKind::Kind,
                RangeOp::Ge,
                RangeValue::U64 { lo: 0, hi: None },
            ),
            range(
                FieldKind::Size,
                RangeOp::Between,
                RangeValue::U64 { lo: 0, hi: None },
            ),
            range(
                FieldKind::Size,
                RangeOp::Between,
                RangeValue::U64 {
                    lo: 10,
                    hi: Some(9),
                },
            ),
            range(
                FieldKind::Created,
                RangeOp::Between,
                RangeValue::I64 {
                    lo: 1,
                    hi: Some(-1),
                },
            ),
            range(
                FieldKind::Size,
                RangeOp::Le,
                RangeValue::U64 {
                    lo: 10,
                    hi: Some(20),
                },
            ),
            literal(FieldKind::Name, "needle", TermModifier::Fuzzy(3)),
            literal(FieldKind::Name, "", TermModifier::Fuzzy(u8::MAX)),
        ];
        for query in invalid {
            // A valid disjunct must not turn the invalid predicate into a
            // successful partial result in either the internal or IPC handler.
            let query = QueryExpr::Or(vec![
                literal(FieldKind::Name, "needle", TermModifier::Term),
                query,
            ]);
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Auto,
                SearchMode::Hybrid,
            ] {
                let req = SearchRequest::with_query(query.clone()).with_mode(mode);
                assert!(fixture.search(&req, &Visibility::default()).is_err());
                let response = fixture.handler.search(req);
                assert!(response.hits.is_empty());
                assert_eq!(response.total, 0);
                assert!(response.truncated);
                assert_eq!(response.served_by.as_deref(), Some("service-unavailable"));
            }
        }
        Ok(())
    }

    #[test]
    fn literal_prefixes_use_field_analysis_and_complete_dictionary_membership() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut expected = Vec::new();
        let mut content_expected = Vec::new();
        // More distinct prefix expansions than Tantivy's default phrase-prefix
        // expansion cap; the exact union must include the complete dictionary.
        for identity in 0..65 {
            let meta = file(
                2,
                identity,
                &format!("Étude{identity}"),
                &format!(r"C:\Projects\Étude{identity}.txt"),
            );
            fixture.put(
                &meta,
                Some(if identity % 2 == 0 {
                    "tailored body"
                } else {
                    "ordinary body"
                }),
            )?;
            expected.push(meta.key);
            if identity % 2 == 0 {
                content_expected.push(meta.key);
            }
        }
        for (identity, name) in [(100, "Étendue"), (101, "Etude")] {
            fixture.put(
                &file(2, identity, name, r"C:\elsewhere\entry.txt"),
                Some("other"),
            )?;
        }
        fixture.commit()?;
        for (field, value) in [(FieldKind::Name, "ÉTU"), (FieldKind::Path, "proj ÉTU")] {
            assert_query_members(
                &fixture,
                &literal(field, value, TermModifier::Prefix),
                &expected,
                &expected,
                &Visibility::default(),
            )?;
        }
        assert_query_members(
            &fixture,
            &literal(FieldKind::Content, "TAIL", TermModifier::Prefix),
            &[],
            &content_expected,
            &Visibility::default(),
        )?;
        let all: Vec<_> = expected
            .iter()
            .copied()
            .chain([DocKey::from_parts(2, 100), DocKey::from_parts(2, 101)])
            .collect();
        assert_query_members(
            &fixture,
            &literal(FieldKind::Ext, "tx", TermModifier::Prefix),
            &all,
            &all,
            &Visibility::default(),
        )?;
        assert_query_members(
            &fixture,
            &literal(FieldKind::Name, "ÉTU OR Etude", TermModifier::Prefix),
            &[],
            &[],
            &Visibility::default(),
        )?;
        Ok(())
    }

    #[test]
    fn literal_phrases_require_analyzed_positions_without_parsing_syntax() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let first = file(2, 1, "blue moon", r"C:\phrase\first.txt");
        let spaced = file(2, 2, "blue distant moon", r"C:\phrase\spaced.txt");
        let reversed = file(2, 3, "moon blue", r"C:\phrase\reversed.txt");
        let syntax = file(2, 4, "name: blue OR content: moon", r"C:\phrase\syntax.txt");
        let prefix = file(2, 5, "blue moonlight", r"C:\phrase\prefix.txt");
        for (meta, body) in [
            (&first, "blue moon shines"),
            (&spaced, "blue distant moon"),
            (&reversed, "moon blue"),
            (&syntax, "name: blue OR content: moon"),
            (&prefix, "blue moonlight"),
        ] {
            fixture.put(meta, Some(body))?;
        }
        fixture.commit()?;
        for (value, expected) in [
            ("BLUE MOON", vec![first.key]),
            ("name: blue OR content: moon", vec![syntax.key]),
            ("!!!", Vec::new()),
        ] {
            assert_query_members(
                &fixture,
                &literal(FieldKind::Name, value, TermModifier::Phrase),
                &expected,
                &expected,
                &Visibility::default(),
            )?;
            assert_query_members(
                &fixture,
                &literal(FieldKind::Content, value, TermModifier::Phrase),
                &[],
                &expected,
                &Visibility::default(),
            )?;
        }
        let all = [first.key, spaced.key, reversed.key, syntax.key, prefix.key];
        assert_query_members(
            &fixture,
            &literal(FieldKind::Ext, "txt", TermModifier::Phrase),
            &all,
            &all,
            &Visibility::default(),
        )?;
        Ok(())
    }

    #[test]
    fn literal_fuzzy_queries_enforce_edit_distance_and_all_analyzed_tokens() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut files = Vec::new();
        for (identity, name) in [
            "report final",
            "reprot finals",
            "reports finale",
            "remote final",
            "report",
            "report finally",
        ]
        .into_iter()
        .enumerate()
        {
            let meta = file(2, identity as u64, name, r"C:\fuzzy\entry.txt");
            fixture.put(&meta, Some(name))?;
            files.push(meta);
        }
        fixture.commit()?;
        for (distance, indices) in [(0, vec![0]), (1, vec![0, 1, 2]), (2, vec![0, 1, 2, 5])] {
            let expected: Vec<_> = indices.iter().map(|&i| files[i].key).collect();
            assert_query_members(
                &fixture,
                &literal(
                    FieldKind::Name,
                    "REPORT FINAL",
                    TermModifier::Fuzzy(distance),
                ),
                &expected,
                &expected,
                &Visibility::default(),
            )?;
            assert_query_members(
                &fixture,
                &literal(
                    FieldKind::Content,
                    "REPORT FINAL",
                    TermModifier::Fuzzy(distance),
                ),
                &[],
                &expected,
                &Visibility::default(),
            )?;
        }
        Ok(())
    }

    #[test]
    fn typed_predicates_keep_exact_union_through_masked_replacement_and_replay() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut changed = file(5, 0xabcd_0000_0000_0010, "report old", r"C:\old\entry.txt");
        let stable = file(5, 20, "report stable", r"C:\stable.txt");
        let child = file(5, 30, "report child", r"C:\covered\child.txt");
        let sibling = file(5, 40, "report sibling", r"C:\coveredness\sibling.txt");
        let other = file(9, 50, "report other", r"D:\other.txt");
        for meta in [&changed, &stable, &child, &sibling, &other] {
            fixture.put(meta, Some("old body"))?;
        }
        fixture.commit()?;
        let query = QueryExpr::And(vec![
            literal(FieldKind::Name, "REP", TermModifier::Prefix),
            range(
                FieldKind::Size,
                RangeOp::Between,
                RangeValue::U64 {
                    lo: 50,
                    hi: Some(150),
                },
            ),
        ]);
        let all = [changed.key, stable.key, child.key, sibling.key, other.key];
        assert_query_members(&fixture, &query, &all, &all, &Visibility::default())?;
        let mut visibility = Visibility::default();
        visibility.documents.insert(changed.key);
        visibility.volumes.insert(9);
        visibility
            .directories
            .insert(5, vec![r"C:\covered".to_string()]);
        changed.name = "report replacement".to_string();
        changed.path = Some(r"C:\new\entry.txt".to_string());
        changed.size = 125;
        fixture.put_content(&changed, Some("new body"))?;
        fixture.content.commit()?;
        fixture.reload()?;
        let masked = [stable.key, sibling.key];
        assert_query_members(&fixture, &query, &masked, &masked, &visibility)?;
        fixture.put_metadata(&changed)?;
        fixture.metadata.commit()?;
        fixture.reload()?;
        visibility.documents.remove(&changed.key);
        let complete = [changed.key, stable.key, sibling.key];
        assert_query_members(&fixture, &query, &complete, &complete, &visibility)?;
        // Replaying the completed update preserves exactly one stable identity.
        fixture.put(&changed, Some("new body"))?;
        fixture.commit()?;
        assert_query_members(&fixture, &query, &complete, &complete, &visibility)?;

        visibility.documents.insert(changed.key);
        changed.size = 200;
        fixture.put_content(&changed, Some("larger body"))?;
        fixture.content.commit()?;
        fixture.reload()?;
        assert_query_members(&fixture, &query, &masked, &masked, &visibility)?;
        fixture.put_metadata(&changed)?;
        fixture.metadata.commit()?;
        fixture.reload()?;
        visibility.documents.remove(&changed.key);
        for _ in 0..2 {
            fixture.delete(stable.key);
            fixture.commit()?;
        }
        fixture.handler = UnifiedSearchHandler::try_new(
            &fixture.root.join("meta"),
            &fixture.root.join("content"),
        )?;
        assert_query_members(
            &fixture,
            &query,
            &[sibling.key],
            &[sibling.key],
            &visibility,
        )?;
        Ok(())
    }

    #[test]
    fn hybrid_pages_count_the_complete_union_with_overlapping_sources() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        for identity in 0..90u64 {
            let name = if identity < 60 {
                "needle candidate"
            } else {
                "plain candidate"
            };
            let meta = file(
                7,
                identity,
                name,
                &format!(r"C:\search\file-{identity}.txt"),
            );
            fixture.put(&meta, (identity >= 30).then_some("needle body"))?;
        }
        fixture.commit()?;
        let visibility = Visibility::default();
        let metadata =
            fixture.search(&request("needle", None, SearchMode::NameOnly), &visibility)?;
        let content = fixture.search(&request("needle", None, SearchMode::Content), &visibility)?;
        assert_eq!(metadata.total, 60);
        assert_eq!(content.total, 60);
        // Independent union oracle from complete per-source results. The hybrid
        // page must retain maximum score and the full-key tie-break even where
        // the two top lists overlap heavily.
        let mut oracle = std::collections::BTreeMap::<DocKey, Score>::new();
        for hit in metadata.hits.into_iter().chain(content.hits) {
            oracle
                .entry(hit.key)
                .and_modify(|score| *score = score.max(hit.score))
                .or_insert(hit.score);
        }
        let mut oracle: Vec<_> = oracle.into_iter().collect();
        oracle.sort_unstable_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        let expected: Vec<_> = oracle.iter().map(|(key, _)| *key).collect();
        assert_eq!(expected.len(), 90);
        for mode in [SearchMode::Auto, SearchMode::Hybrid] {
            let mut combined = Vec::new();
            for offset in (0..98).step_by(7) {
                let req = request("needle", None, mode)
                    .with_limit(7)
                    .with_offset(offset);
                let page = fixture.search(&req, &visibility)?;
                assert_eq!(
                    page.total, 90,
                    "total must not shrink to the candidate window"
                );
                assert!(!page.truncated);
                assert_eq!(
                    keys(&page),
                    expected
                        .iter()
                        .copied()
                        .skip(offset as usize)
                        .take(7)
                        .collect::<Vec<_>>()
                );
                for hit in &page.hits {
                    assert_eq!(
                        hit.score,
                        oracle.iter().find(|(key, _)| *key == hit.key).unwrap().1
                    );
                }
                combined.extend(keys(&page));
            }
            assert_eq!(combined, expected);
            assert_eq!(combined.iter().copied().collect::<BTreeSet<_>>().len(), 90);
        }
        Ok(())
    }

    #[test]
    fn tied_pages_keep_full_identity_order_across_replacement_merge_and_reopen() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut files = Vec::new();
        for volume in [10, 2] {
            for identity in (0..35).rev() {
                let reference = 0xabcd_0000_0000_0000 | identity;
                files.push(file(volume, reference, "needle", r"C:\ties\entry.txt"));
            }
        }
        for page in files.chunks(11) {
            for meta in page {
                fixture.put(meta, Some("plain"))?;
            }
            fixture.commit()?;
        }
        let mut expected: Vec<_> = files.iter().map(|meta| meta.key).collect();
        expected.sort_unstable();
        let assert_pages = |fixture: &SearchFixture| -> Result<()> {
            for mode in [SearchMode::Auto, SearchMode::NameOnly, SearchMode::Content] {
                let mut actual = Vec::new();
                for offset in (0..70).step_by(3) {
                    let page = fixture.search(
                        &request("needle", Some(FieldKind::Name), mode)
                            .with_limit(3)
                            .with_offset(offset),
                        &Visibility::default(),
                    )?;
                    assert_eq!(page.total, 70);
                    actual.extend(keys(&page));
                }
                assert_eq!(actual, expected);
            }
            Ok(())
        };
        assert_pages(&fixture)?;
        for page in files.iter().rev().collect::<Vec<_>>().chunks(13) {
            for meta in page {
                fixture.put(meta, Some("plain"))?;
            }
            fixture.commit()?;
        }
        let segments = fixture.handler.meta.index.searchable_segment_ids()?;
        fixture.metadata.merge(&segments).wait()?;
        let segments = fixture
            .handler
            .content
            .as_ref()
            .unwrap()
            .0
            .index
            .searchable_segment_ids()?;
        fixture.content.merge(&segments).wait()?;
        fixture.handler = UnifiedSearchHandler::try_new(
            &fixture.root.join("meta"),
            &fixture.root.join("content"),
        )?;
        assert_pages(&fixture)?;
        Ok(())
    }

    #[test]
    fn hybrid_union_remains_exact_during_masked_replacement_and_delete_replay() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let original = file(5, 1, "needle original", r"C:\before\original.txt");
        let stable = file(5, 2, "needle stable", r"C:\stable.txt");
        let deleted = file(5, 3, "plain", r"C:\deleted.txt");
        let metadata_only = file(5, 4, "needle metadata", r"C:\metadata.txt");
        for (meta, body) in [
            (&original, Some("obsolete")),
            (&stable, Some("needle steady")),
            (&deleted, Some("needle")),
            (&metadata_only, None),
        ] {
            fixture.put(meta, body)?;
        }
        fixture.commit()?;
        let query = request("needle", None, SearchMode::Auto);
        assert_eq!(fixture.search(&query, &Visibility::default())?.total, 4);

        let mut replacement = original.clone();
        replacement.name = "renamed survivor".into();
        replacement.path = Some(r"C:\after\renamed.txt".into());
        replacement.size = 123;
        replacement.modified = 900;
        fixture.put_content(&replacement, Some("fresh body"))?;
        fixture.content.commit()?;
        fixture.reload()?;
        let mut pending = Visibility::default();
        pending.documents.insert(original.key);
        let masked = fixture.search(&query, &pending)?;
        assert_eq!(masked.total, 3);
        assert!(!keys(&masked).contains(&original.key));
        assert_eq!(
            fixture
                .search(&request("fresh", None, SearchMode::Auto), &pending)?
                .total,
            0
        );

        fixture.put_metadata(&replacement)?;
        fixture.metadata.commit()?;
        for _ in 0..2 {
            fixture.put(&replacement, Some("fresh body"))?;
            fixture.delete(deleted.key);
            fixture.commit()?;
        }
        let complete = fixture.search(&query, &Visibility::default())?;
        assert_eq!(complete.total, 2);
        assert_eq!(
            keys(&complete).into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([stable.key, metadata_only.key])
        );
        assert_eq!(
            fixture
                .search(
                    &request("obsolete", None, SearchMode::Auto),
                    &Visibility::default()
                )?
                .total,
            0
        );
        let fresh = fixture.search(
            &request("fresh", None, SearchMode::Auto),
            &Visibility::default(),
        )?;
        assert_eq!(fresh.total, 1);
        assert_eq!(keys(&fresh), vec![original.key]);
        assert_eq!(fresh.hits[0].path, replacement.path);
        assert_eq!(fresh.hits[0].size, Some(123));
        assert_eq!(fresh.hits[0].modified, Some(900));
        Ok(())
    }

    #[test]
    fn hybrid_counts_apply_document_volume_and_directory_masks_before_paging() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let masked = file(3, 1, "needle", r"C:\root\Moved\child.txt");
        let sibling = file(3, 2, "needle", r"C:\root\Moved-elsewhere\child.txt");
        let other_volume = file(4, 3, "needle", r"C:\root\Moved\child.txt");
        let masked_doc = file(3, 4, "needle", r"C:\root\clear.txt");
        let hidden_volume = file(8, 5, "needle", r"C:\root\another.txt");
        for meta in [
            &masked,
            &sibling,
            &other_volume,
            &masked_doc,
            &hidden_volume,
        ] {
            fixture.put(meta, Some("needle"))?;
        }
        fixture.commit()?;
        let mut visibility = Visibility::default();
        visibility
            .directories
            .insert(3, vec!["c:/ROOT/moved".into()]);
        visibility.documents.insert(masked_doc.key);
        visibility.volumes.insert(hidden_volume.volume);
        for mode in [
            SearchMode::Auto,
            SearchMode::Hybrid,
            SearchMode::NameOnly,
            SearchMode::Content,
        ] {
            let req = request("needle", None, mode).with_limit(1);
            let first = fixture.search(&req, &visibility)?;
            let second = fixture.search(&req.with_offset(1), &visibility)?;
            assert_eq!(first.total, 2);
            assert_eq!(second.total, 2);
            assert_eq!(first.hits.len(), 1);
            assert_eq!(second.hits.len(), 1);
            assert_eq!(
                keys(&first)
                    .into_iter()
                    .chain(keys(&second))
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([sibling.key, other_volume.key])
            );
        }
        Ok(())
    }

    #[test]
    fn hybrid_count_preserves_metadata_parser_field_resolution() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        fixture.put(&file(2, 1, "plain", r"C:\plain.txt"), Some("needle"))?;
        fixture.commit()?;
        // This syntax names a field absent from the metadata parser's schema.
        // Re-parsing against the content schema when calculating overlap would
        // incorrectly treat this content-only match as already counted there.
        let req = request("content:needle", Some(FieldKind::Name), SearchMode::Auto);
        let response = fixture.search(&req, &Visibility::default())?;
        assert_eq!(response.total, 1);
        assert_eq!(response.hits.len(), 1);
        Ok(())
    }

    #[test]
    fn oversized_windows_are_bounded_and_report_exact_totals_with_truncation() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        for identity in 0..MAX_SEARCH_PAGE as u64 + 5 {
            fixture.put(
                &file(9, identity, "needle", r"C:\bounded.txt"),
                Some("plain"),
            )?;
        }
        fixture.commit()?;
        for mode in [SearchMode::Auto, SearchMode::NameOnly, SearchMode::Content] {
            let req = request("needle", Some(FieldKind::Name), mode).with_limit(u32::MAX);
            let response = fixture.search(&req, &Visibility::default())?;
            assert_eq!(response.hits.len(), MAX_SEARCH_PAGE);
            assert_eq!(response.total, MAX_SEARCH_PAGE as u64 + 5);
            assert!(response.truncated);
            let beyond = fixture.search(&req.with_offset(u32::MAX), &Visibility::default())?;
            assert!(beyond.hits.is_empty());
            assert_eq!(beyond.total, MAX_SEARCH_PAGE as u64 + 5);
            assert!(
                beyond.truncated,
                "an unserved window must never look exhausted"
            );
        }
        let edge = SearchWindow::for_request(
            &request("needle", None, SearchMode::Auto)
                .with_offset((MAX_SEARCH_WINDOW - 3) as u32)
                .with_limit(7),
        );
        assert_eq!(edge.candidates, MAX_SEARCH_WINDOW);
        assert_eq!(edge.limit, 3);
        assert!(edge.truncated);
        Ok(())
    }

    #[test]
    fn hybrid_collector_failure_cannot_become_successful_partial_results() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let good = file(65534, 1, "unverifiableuniquetoken", r"C:\healthy.txt");
        fixture.put(&good, Some("plain"))?;
        let fields = &fixture.handler.content.as_ref().unwrap().0.fields;
        // A corrupt content row matches the query but has no stable identity.
        // Metadata already has a valid hit; returning that subset would hide
        // the broken source and fabricate a successful union response.
        fixture.content.add_document(tantivy::doc!(
            fields.volume => 65534u64,
            fields.name => "unverifiableuniquetoken",
            fields.path => r"C:\invalid.txt",
            fields.content => "plain"
        ))?;
        fixture.commit()?;
        let req = request("unverifiableuniquetoken", None, SearchMode::Auto);
        assert!(fixture.search(&req, &Visibility::default()).is_err());
        let response = fixture.handler.search(req);
        assert!(response.hits.is_empty());
        assert_eq!(response.served_by.as_deref(), Some("service-unavailable"));
        assert!(response.truncated);
        Ok(())
    }

    #[test]
    fn pure_and_nested_negation_across_all_modes_pagination_and_visibility() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut alpha_meta = file(2, 1, "alpha report", r"C:\docs\alpha.txt");
        alpha_meta.size = 50;
        let mut beta_both = file(2, 2, "beta summary", r"C:\docs\beta.txt");
        beta_both.size = 150;
        let mut gamma_secret = file(2, 3, "gamma memo", r"C:\docs\gamma.txt");
        gamma_secret.size = 250;
        let mut delta_moved = file(3, 4, "delta notes", r"C:\Archive\Sub\delta.txt");
        delta_moved.size = 350;

        fixture.put(&alpha_meta, None)?;
        fixture.put(&beta_both, Some("ordinary body"))?;
        fixture.put(&gamma_secret, Some("secret classified payload"))?;
        fixture.put(&delta_moved, Some("ordinary archive body"))?;
        fixture.commit()?;

        let vis = Visibility::default();

        // 1. Pure metadata negation: Not(Name("alpha")) matches beta, gamma, delta in metadata
        // and beta, gamma, delta in content.
        let not_alpha = QueryExpr::Not(Box::new(literal(
            FieldKind::Name,
            "alpha",
            TermModifier::Term,
        )));
        assert_query_members(
            &fixture,
            &not_alpha,
            &[beta_both.key, gamma_secret.key, delta_moved.key],
            &[beta_both.key, gamma_secret.key, delta_moved.key],
            &vis,
        )?;

        // 2. Pure content negation: Not(Content("secret")) is empty in metadata-only index
        // and matches beta + delta in content index (and therefore Auto/Hybrid).
        let not_secret = QueryExpr::Not(Box::new(literal(
            FieldKind::Content,
            "secret",
            TermModifier::Term,
        )));
        assert_query_members(
            &fixture,
            &not_secret,
            &[],
            &[beta_both.key, delta_moved.key],
            &vis,
        )?;

        // 3. Unfielded negation: Not(Term(None, "secret")) must not match gamma_secret via
        // metadata index when running Auto/Hybrid.
        let not_unfielded_secret = QueryExpr::Not(Box::new(QueryExpr::Term(TermExpr {
            field: None,
            value: "secret".into(),
            modifier: TermModifier::Term,
        })));
        assert_query_members(
            &fixture,
            &not_unfielded_secret,
            &[],
            &[beta_both.key, delta_moved.key],
            &vis,
        )?;

        // 4. Raw negative query "-beta" on Name field vs unfielded:
        let raw_not_beta_name = literal(FieldKind::Name, "-beta", TermModifier::Term);
        assert_query_members(
            &fixture,
            &raw_not_beta_name,
            &[alpha_meta.key, gamma_secret.key, delta_moved.key],
            &[gamma_secret.key, delta_moved.key],
            &vis,
        )?;

        // 5. Negated range: Not(Size >= 200) -> alpha (50, meta-only) and beta (150, both).
        let not_large = QueryExpr::Not(Box::new(range(
            FieldKind::Size,
            RangeOp::Ge,
            RangeValue::U64 { lo: 200, hi: None },
        )));
        assert_query_members(
            &fixture,
            &not_large,
            &[alpha_meta.key, beta_both.key],
            &[beta_both.key],
            &vis,
        )?;

        // 6. Double negation & De Morgan: Not(Not(Name("beta"))) -> beta_both
        let double_neg = QueryExpr::Not(Box::new(QueryExpr::Not(Box::new(literal(
            FieldKind::Name,
            "beta",
            TermModifier::Term,
        )))));
        assert_query_members(
            &fixture,
            &double_neg,
            &[beta_both.key],
            &[beta_both.key],
            &vis,
        )?;

        // 7. Nested De Morgan with mixed positive/negative clauses + visibility + pagination:
        // Not(Or([Name("alpha"), Content("secret")])) -> And([Not(Name("alpha")), Not(Content("secret"))])
        let neither_alpha_nor_secret = QueryExpr::Not(Box::new(QueryExpr::Or(vec![
            literal(FieldKind::Name, "alpha", TermModifier::Term),
            literal(FieldKind::Content, "secret", TermModifier::Term),
        ])));
        assert_query_members(
            &fixture,
            &neither_alpha_nor_secret,
            &[],
            &[beta_both.key, delta_moved.key],
            &vis,
        )?;

        let mut masked_vis = Visibility::default();
        masked_vis
            .directories
            .insert(3, vec!["c:/archive/sub".into()]);
        assert_query_members(
            &fixture,
            &neither_alpha_nor_secret,
            &[],
            &[beta_both.key],
            &masked_vis,
        )?;

        for mode in [SearchMode::Auto, SearchMode::Hybrid, SearchMode::NameOnly] {
            let page1 = fixture.search(
                &SearchRequest::with_query(not_alpha.clone())
                    .with_mode(mode)
                    .with_limit(2)
                    .with_offset(0),
                &vis,
            )?;
            let page2 = fixture.search(
                &SearchRequest::with_query(not_alpha.clone())
                    .with_mode(mode)
                    .with_limit(2)
                    .with_offset(2),
                &vis,
            )?;
            assert_eq!(page1.total, 3);
            assert_eq!(page2.total, 3);
            assert_eq!(page1.hits.len(), 2);
            assert_eq!(page2.hits.len(), 1);
        }

        Ok(())
    }

    #[test]
    fn malformed_raw_and_numeric_term_queries_reject_the_whole_request() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        fixture.put(&file(2, 1, "needle", r"C:\needle.txt"), Some("needle"))?;
        fixture.commit()?;

        let invalid = [
            QueryExpr::Term(TermExpr {
                field: None,
                value: "[unclosed".into(),
                modifier: TermModifier::Term,
            }),
            QueryExpr::Term(TermExpr {
                field: None,
                value: "unknown_field:needle".into(),
                modifier: TermModifier::Term,
            }),
            literal(FieldKind::Size, "100", TermModifier::Term),
            literal(FieldKind::Modified, "200", TermModifier::Term),
            literal(FieldKind::Created, "100", TermModifier::Prefix),
            literal(FieldKind::Kind, "nonexistent_kind", TermModifier::Term),
        ];

        for bad in invalid {
            for mode in [
                SearchMode::NameOnly,
                SearchMode::Content,
                SearchMode::Auto,
                SearchMode::Hybrid,
            ] {
                let req = SearchRequest::with_query(bad.clone()).with_mode(mode);
                assert!(
                    fixture.search(&req, &Visibility::default()).is_err(),
                    "expected error for {bad:?} in {mode:?}"
                );
                let resp = fixture.handler.search(req);
                assert!(resp.hits.is_empty());
                assert!(resp.truncated);
                assert_eq!(resp.served_by.as_deref(), Some("service-unavailable"));
            }
        }
        Ok(())
    }

    #[test]
    fn field_kind_expansion_and_exact_content_extension_matching() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let mut rust_file = file(2, 1, "lib.rs", r"C:\src\lib.rs");
        rust_file.ext = Some("rs".into());
        let mut pdf_file = file(2, 2, "manual.pdf", r"C:\docs\manual.pdf");
        pdf_file.ext = Some("pdf".into());
        let mut special_ext = file(2, 3, "template.c++", r"C:\src\template.c++");
        special_ext.ext = Some("c++".into());

        fixture.put(&rust_file, Some("fn main() {}"))?;
        fixture.put(&pdf_file, Some("user guide"))?;
        fixture.put(&special_ext, Some("template class"))?;
        fixture.commit()?;

        let vis = Visibility::default();
        assert_query_members(
            &fixture,
            &literal(FieldKind::Kind, "code", TermModifier::Term),
            &[rust_file.key],
            &[rust_file.key],
            &vis,
        )?;
        assert_query_members(
            &fixture,
            &literal(FieldKind::Kind, "docs", TermModifier::Term),
            &[pdf_file.key],
            &[pdf_file.key],
            &vis,
        )?;
        assert_query_members(
            &fixture,
            &literal(FieldKind::Ext, "c++", TermModifier::Term),
            &[special_ext.key],
            &[special_ext.key],
            &vis,
        )?;
        Ok(())
    }

    #[test]
    fn content_and_hybrid_hits_populate_bounded_snippets() -> Result<()> {
        let mut fixture = SearchFixture::new()?;
        let doc = file(2, 1, "architecture.md", r"C:\docs\architecture.md");
        let body = format!(
            "{} ultrasearch_unique_token_xyz {}",
            "prefix words ".repeat(10),
            "suffix words ".repeat(30)
        );
        fixture.put(&doc, Some(&body))?;
        fixture.commit()?;

        for mode in [SearchMode::Content, SearchMode::Auto, SearchMode::Hybrid] {
            let resp = fixture.search(
                &request(
                    "ultrasearch_unique_token_xyz",
                    Some(FieldKind::Content),
                    mode,
                ),
                &Visibility::default(),
            )?;
            assert_eq!(resp.hits.len(), 1);
            let snippet = resp.hits[0]
                .snippet
                .as_deref()
                .expect("content hit should populate snippet");
            assert!(
                snippet.contains("ultrasearch_unique_token_xyz"),
                "snippet should contain matched token: {snippet}"
            );
            assert!(snippet.chars().count() <= 203);
        }

        // Metadata match on a content-indexed file in Hybrid/Auto should also populate fallback excerpt
        let resp = fixture.search(
            &request("architecture", Some(FieldKind::Name), SearchMode::Hybrid),
            &Visibility::default(),
        )?;
        assert_eq!(resp.hits.len(), 1);
        assert!(resp.hits[0].snippet.is_some());
        Ok(())
    }
}
