//! IPC protocol models for UltraSearch.
//!
//! These types are serialized with bincode over a length-prefixed pipe
//! framing (handled in the service/CLI/UI). The goal here is to model the
//! query AST, requests, and responses in a way that matches the architecture
//! plan without pulling in search/index dependencies.

use core_types::DocKey;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

/// Serialize Durations as milliseconds to keep the wire format stable even if serde defaults change.
mod duration_ms {
    use serde::{Deserialize, Deserializer, Serializer, de::IntoDeserializer};
    use std::time::Duration;

    #[allow(dead_code)]
    pub fn serialize<S>(dur: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let ms: u64 = dur
            .as_millis()
            .try_into()
            .map_err(|_| serde::ser::Error::custom("duration too large for u64 millis"))?;
        serializer.serialize_u64(ms)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(ms))
    }

    pub mod option {
        use super::*;
        use serde::{Deserializer, Serializer};

        pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match value {
                Some(dur) => {
                    let ms: u64 = dur.as_millis().try_into().map_err(|_| {
                        serde::ser::Error::custom("duration too large for u64 millis")
                    })?;
                    serializer.serialize_some(&ms)
                }
                None => serializer.serialize_none(),
            }
        }

        pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
        where
            D: Deserializer<'de>,
        {
            match Option::<u64>::deserialize(deserializer)? {
                Some(ms) => super::deserialize(ms.into_deserializer()).map(Some),
                None => Ok(None),
            }
        }
    }
}

/// Fields that can be targeted explicitly in the query language.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FieldKind {
    Name,
    Path,
    Ext,
    Content,
    Size,
    Modified,
    Created,
    Flags,
    Volume,
    Kind,
}

/// How a term should be interpreted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TermModifier {
    Term,
    Phrase,
    Prefix,
    Fuzzy(u8), // max edit distance
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermExpr {
    pub field: Option<FieldKind>, // None => default (name + content)
    pub value: String,
    pub modifier: TermModifier,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
    Between,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RangeValue {
    I64 { lo: i64, hi: Option<i64> }, // timestamps
    U64 { lo: u64, hi: Option<u64> }, // sizes
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeExpr {
    pub field: FieldKind,
    pub op: RangeOp,
    pub value: RangeValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum QueryExpr {
    Term(TermExpr),
    Range(RangeExpr),
    Not(Box<QueryExpr>),
    And(Vec<QueryExpr>),
    Or(Vec<QueryExpr>),
}

impl Default for QueryExpr {
    fn default() -> Self {
        QueryExpr::And(Vec::new())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Auto, // planner decides
    NameOnly, // metadata index only
    Content,  // content index
    Hybrid,   // meta + content merge
}

#[cfg(windows)]
pub mod client;
pub mod framing;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub id: Uuid,
    #[serde(default)]
    pub query: QueryExpr,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default, with = "duration_ms::option")]
    pub timeout: Option<Duration>,
    #[serde(default)]
    pub offset: u32,
}

fn default_limit() -> u32 {
    50
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            id: Uuid::nil(),
            query: QueryExpr::default(),
            limit: 50,
            mode: SearchMode::Auto,
            timeout: None,
            offset: 0,
        }
    }
}

impl SearchRequest {
    /// Convenience constructor with a supplied query.
    pub fn with_query(query: QueryExpr) -> Self {
        Self {
            query,
            ..Default::default()
        }
    }

    /// Set a timeout in milliseconds.
    pub fn with_timeout_ms(mut self, ms: u64) -> Self {
        self.timeout = Some(Duration::from_millis(ms));
        self
    }

    /// Set a result limit.
    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    /// Set paging offset.
    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    /// Override the search mode.
    pub fn with_mode(mut self, mode: SearchMode) -> Self {
        self.mode = mode;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub key: DocKey,
    pub score: f32,
    pub name: Option<String>,
    pub path: Option<String>,
    pub ext: Option<String>,
    pub size: Option<u64>,
    pub modified: Option<i64>,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub id: Uuid,
    pub hits: Vec<SearchHit>,
    pub total: u64,
    pub truncated: bool,
    pub took_ms: u32,
    #[serde(default)]
    pub served_by: Option<String>,
}

impl SearchResponse {
    /// Validate a decoded search before a client publishes its rows or count.
    /// The service uses its unavailable label for failed query execution and
    /// failed response serialization; neither is a successful empty search.
    /// Truncation alone remains valid, including count-only and bounded pages.
    pub fn validate_for(&self, request_id: Uuid) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.id == request_id,
            "IPC search response ID {} does not match request {}",
            self.id,
            request_id
        );
        anyhow::ensure!(
            self.served_by.as_deref() != Some("service-unavailable"),
            "UltraSearch could not complete this search; retry or check the service logs"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequest {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadConfigRequest {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadConfigResponse {
    pub id: Uuid,
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RescanRequest {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RescanResponse {
    pub id: Uuid,
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub volume: u16,
    pub indexed_files: u64,
    pub indexed_bytes: u64,
    pub pending_files: u64,
    pub pending_bytes: u64,
    pub last_usn: Option<u64>,
    pub journal_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub id: Uuid,
    pub volumes: Vec<VolumeStatus>,
    pub last_index_commit_ts: Option<i64>,
    pub scheduler_state: String,
    pub content_jobs_total: Option<u64>,
    pub content_jobs_remaining: Option<u64>,
    pub content_bytes_total: Option<u64>,
    pub content_bytes_remaining: Option<u64>,
    pub metrics: Option<MetricsSnapshot>,
    pub served_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub search_latency_ms_p50: Option<f64>,
    pub search_latency_ms_p95: Option<f64>,
    pub worker_cpu_pct: Option<f64>,
    pub worker_mem_bytes: Option<u64>,
    pub queue_depth: Option<u64>,
    pub active_workers: Option<u32>,
    /// Total content jobs enqueued since startup (best-effort).
    pub content_enqueued: Option<u64>,
    /// Total content jobs dropped due to backpressure or missing scheduler (best-effort).
    pub content_dropped: Option<u64>,
}

/// Returns `true` when `input` contains human-friendly filter qualifiers or
/// natural-language range tokens (such as `ext:`, `kind:`, `size:`,
/// `modified:`, `created:`, `date:`, `dm:`, `dc:`, `vol:`, `folder:`, `dir:`,
/// `file:`, `type:`, `in:`, `body:`) that should be lowered into a structured
/// [`QueryExpr`] AST rather than passed as a single raw Tantivy term.
pub fn has_human_filter_tokens(input: &str) -> bool {
    const HUMAN_PREFIXES: &[&str] = &[
        "ext:",
        "extension:",
        "kind:",
        "type:",
        "category:",
        "size:",
        "len:",
        "length:",
        "modified:",
        "dm:",
        "mtime:",
        "date:",
        "created:",
        "dc:",
        "ctime:",
        "vol:",
        "volume:",
        "flags:",
        "flag:",
        "folder:",
        "dir:",
        "directory:",
        "file:",
        "filename:",
        "in:",
        "body:",
        "text:",
    ];

    for raw_tok in input.split_whitespace() {
        let tok = raw_tok
            .trim_start_matches(['(', '-', '!'])
            .to_ascii_lowercase();
        if HUMAN_PREFIXES
            .iter()
            .any(|prefix| tok.starts_with(prefix) && tok.len() > prefix.len())
        {
            return true;
        }
    }
    false
}

/// Parse a user-facing search string into a structured [`QueryExpr`] using the
/// current system UTC clock for relative date expressions (`today`, `yesterday`,
/// `last week`, `7d`, etc.).
pub fn parse_query_str(input: &str) -> QueryExpr {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    parse_user_query(input, now)
}

/// Parse a user-facing search string into a structured [`QueryExpr`] relative
/// to `now_unix_secs` (UTC Unix timestamp in seconds).
pub fn parse_user_query(input: &str, now_unix_secs: i64) -> QueryExpr {
    let tokens = tokenize_user_query(input);
    if tokens.is_empty() {
        return QueryExpr::And(Vec::new());
    }
    let mut pos = 0;
    let expr = parse_or_expr(&tokens, &mut pos, now_unix_secs);
    expr.unwrap_or_else(|| QueryExpr::And(Vec::new()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawToken {
    LParen,
    RParen,
    Atom { text: String, quoted: bool },
}

fn tokenize_user_query(input: &str) -> Vec<RawToken> {
    let mut raw_tokens = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch == '(' {
            chars.next();
            raw_tokens.push(RawToken::LParen);
            continue;
        }
        if ch == ')' {
            chars.next();
            raw_tokens.push(RawToken::RParen);
            continue;
        }
        if ch == '"' {
            chars.next();
            let mut buf = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        buf.push(escaped);
                    }
                } else if c == '"' {
                    break;
                } else {
                    buf.push(c);
                }
            }
            if !buf.is_empty() {
                raw_tokens.push(RawToken::Atom {
                    text: buf,
                    quoted: true,
                });
            }
            continue;
        }

        let mut buf = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '(' || c == ')' {
                break;
            }
            if c == '"' {
                chars.next();
                buf.push('"');
                while let Some(qc) = chars.next() {
                    if qc == '\\' {
                        if let Some(escaped) = chars.next() {
                            buf.push(escaped);
                        }
                    } else if qc == '"' {
                        buf.push('"');
                        break;
                    } else {
                        buf.push(qc);
                    }
                }
            } else {
                buf.push(c);
                chars.next();
            }
        }
        if !buf.is_empty() {
            raw_tokens.push(RawToken::Atom {
                text: buf,
                quoted: false,
            });
        }
    }

    // Coalesce multi-word relative date expressions such as `date:last week`,
    // `modified:this month`, or `created:past year` into a single atom.
    let mut merged = Vec::with_capacity(raw_tokens.len());
    let mut idx = 0;
    while idx < raw_tokens.len() {
        if idx + 1 < raw_tokens.len()
            && let (
                RawToken::Atom {
                    text: first,
                    quoted: false,
                },
                RawToken::Atom {
                    text: second,
                    quoted: false,
                },
            ) = (&raw_tokens[idx], &raw_tokens[idx + 1])
            && let Some((prefix, rest)) = first.split_once(':')
            && matches!(
                prefix
                    .trim_start_matches(['-', '!'])
                    .to_ascii_lowercase()
                    .as_str(),
                "modified" | "dm" | "mtime" | "date" | "created" | "dc" | "ctime"
            )
            && matches!(rest.to_ascii_lowercase().as_str(), "last" | "this" | "past")
            && matches!(
                second.to_ascii_lowercase().as_str(),
                "day" | "days" | "week" | "weeks" | "month" | "months" | "year" | "years"
            )
        {
            merged.push(RawToken::Atom {
                text: format!("{prefix}:{rest}{second}"),
                quoted: false,
            });
            idx += 2;
            continue;
        }
        merged.push(raw_tokens[idx].clone());
        idx += 1;
    }

    merged
}

fn parse_or_expr(tokens: &[RawToken], pos: &mut usize, now: i64) -> Option<QueryExpr> {
    let mut branches = Vec::new();
    if let Some(first) = parse_and_expr(tokens, pos, now) {
        branches.push(first);
    }
    while *pos < tokens.len() {
        if matches!(
            &tokens[*pos],
            RawToken::Atom { text, quoted: false } if text.eq_ignore_ascii_case("OR")
        ) {
            *pos += 1;
            if let Some(rhs) = parse_and_expr(tokens, pos, now) {
                branches.push(rhs);
            }
        } else {
            break;
        }
    }
    match branches.len() {
        0 => None,
        1 => branches.pop(),
        _ => Some(QueryExpr::Or(branches)),
    }
}

fn parse_and_expr(tokens: &[RawToken], pos: &mut usize, now: i64) -> Option<QueryExpr> {
    let mut clauses = Vec::new();
    while *pos < tokens.len() {
        match &tokens[*pos] {
            RawToken::RParen => break,
            RawToken::Atom {
                text,
                quoted: false,
            } if text.eq_ignore_ascii_case("OR") => break,
            RawToken::Atom {
                text,
                quoted: false,
            } if text.eq_ignore_ascii_case("AND") => {
                *pos += 1;
                continue;
            }
            _ => {
                if let Some(clause) = parse_unary_expr(tokens, pos, now) {
                    clauses.push(clause);
                } else {
                    break;
                }
            }
        }
    }
    match clauses.len() {
        0 => None,
        1 => clauses.pop(),
        _ => Some(QueryExpr::And(clauses)),
    }
}

fn parse_unary_expr(tokens: &[RawToken], pos: &mut usize, now: i64) -> Option<QueryExpr> {
    if *pos >= tokens.len() {
        return None;
    }
    match &tokens[*pos] {
        RawToken::Atom {
            text,
            quoted: false,
        } if text.eq_ignore_ascii_case("NOT") => {
            *pos += 1;
            let inner = parse_unary_expr(tokens, pos, now)?;
            Some(QueryExpr::Not(Box::new(inner)))
        }
        RawToken::LParen => {
            *pos += 1;
            let inner = parse_or_expr(tokens, pos, now);
            if *pos < tokens.len() && matches!(tokens[*pos], RawToken::RParen) {
                *pos += 1;
            }
            inner
        }
        RawToken::RParen => None,
        RawToken::Atom { text, quoted } => {
            let text = text.clone();
            let quoted = *quoted;
            *pos += 1;
            if !quoted
                && let Some(stripped) = text.strip_prefix('-').or_else(|| text.strip_prefix('!'))
                && !stripped.is_empty()
            {
                if stripped.starts_with('"') && stripped.ends_with('"') && stripped.len() >= 2 {
                    return Some(QueryExpr::Not(Box::new(QueryExpr::Term(TermExpr {
                        field: None,
                        value: stripped[1..stripped.len() - 1].to_string(),
                        modifier: TermModifier::Phrase,
                    }))));
                }
                return Some(QueryExpr::Not(Box::new(parse_atom_token(
                    stripped, false, now,
                ))));
            }
            Some(parse_atom_token(&text, quoted, now))
        }
    }
}

fn parse_atom_token(text: &str, quoted: bool, now: i64) -> QueryExpr {
    if quoted {
        return QueryExpr::Term(TermExpr {
            field: None,
            value: text.to_string(),
            modifier: TermModifier::Phrase,
        });
    }

    if let Some((raw_prefix, raw_rest)) = text.split_once(':') {
        let rest = raw_rest.trim();
        if !rest.is_empty() {
            let prefix_lower = raw_prefix.to_ascii_lowercase();
            match prefix_lower.as_str() {
                "name" | "file" | "filename" => {
                    return parse_field_term(Some(FieldKind::Name), rest);
                }
                "path" | "folder" | "dir" | "directory" => {
                    return parse_field_term(Some(FieldKind::Path), rest);
                }
                "content" | "body" | "in" | "text" => {
                    return parse_field_term(Some(FieldKind::Content), rest);
                }
                "ext" | "extension" => {
                    let unquoted = strip_surrounding_quotes(rest);
                    let parts: Vec<QueryExpr> = unquoted
                        .split(',')
                        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .map(|ext| {
                            QueryExpr::Term(TermExpr {
                                field: Some(FieldKind::Ext),
                                value: ext,
                                modifier: TermModifier::Term,
                            })
                        })
                        .collect();
                    if parts.len() == 1 {
                        return parts.into_iter().next().unwrap();
                    } else if parts.len() > 1 {
                        return QueryExpr::Or(parts);
                    }
                }
                "kind" | "type" | "category" => {
                    let unquoted = strip_surrounding_quotes(rest);
                    let parts: Vec<QueryExpr> = unquoted
                        .split(',')
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .map(|k| {
                            QueryExpr::Term(TermExpr {
                                field: Some(FieldKind::Kind),
                                value: k,
                                modifier: TermModifier::Term,
                            })
                        })
                        .collect();
                    if parts.len() == 1 {
                        return parts.into_iter().next().unwrap();
                    } else if parts.len() > 1 {
                        return QueryExpr::Or(parts);
                    }
                }
                "size" | "len" | "length" => {
                    if let Some(range) = parse_size_range(strip_surrounding_quotes(rest)) {
                        return QueryExpr::Range(range);
                    }
                }
                "modified" | "dm" | "mtime" | "date" => {
                    if let Some(range) =
                        parse_date_range(FieldKind::Modified, strip_surrounding_quotes(rest), now)
                    {
                        return QueryExpr::Range(range);
                    }
                }
                "created" | "dc" | "ctime" => {
                    if let Some(range) =
                        parse_date_range(FieldKind::Created, strip_surrounding_quotes(rest), now)
                    {
                        return QueryExpr::Range(range);
                    }
                }
                "vol" | "volume" => {
                    if let Some(range) =
                        parse_u64_range(FieldKind::Volume, strip_surrounding_quotes(rest))
                    {
                        return QueryExpr::Range(range);
                    }
                }
                "flags" | "flag" => {
                    if let Some(range) =
                        parse_u64_range(FieldKind::Flags, strip_surrounding_quotes(rest))
                    {
                        return QueryExpr::Range(range);
                    }
                }
                _ => {}
            }
        }
    }

    parse_field_term(None, text)
}

fn strip_surrounding_quotes(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn parse_field_term(field: Option<FieldKind>, raw: &str) -> QueryExpr {
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        return QueryExpr::Term(TermExpr {
            field,
            value: raw[1..raw.len() - 1].to_string(),
            modifier: TermModifier::Phrase,
        });
    }
    if let Some(stem) = raw.strip_suffix('*')
        && !stem.is_empty()
        && !stem.contains('*')
    {
        return QueryExpr::Term(TermExpr {
            field,
            value: stem.to_string(),
            modifier: TermModifier::Prefix,
        });
    }
    if let Some((stem, dist_str)) = raw.rsplit_once('~')
        && !stem.is_empty()
    {
        if dist_str.is_empty() {
            return QueryExpr::Term(TermExpr {
                field,
                value: stem.to_string(),
                modifier: TermModifier::Fuzzy(1),
            });
        } else if let Ok(dist) = dist_str.parse::<u8>() {
            return QueryExpr::Term(TermExpr {
                field,
                value: stem.to_string(),
                modifier: TermModifier::Fuzzy(dist.min(2)),
            });
        }
    }
    QueryExpr::Term(TermExpr {
        field,
        value: raw.to_string(),
        modifier: TermModifier::Term,
    })
}

fn parse_u64_range(field: FieldKind, spec: &str) -> Option<RangeExpr> {
    let s = spec.trim();
    if let Some((lo_s, hi_s)) = s.split_once("..") {
        let lo = lo_s.trim().parse::<u64>().ok()?;
        let hi = hi_s.trim().parse::<u64>().ok()?;
        return Some(RangeExpr {
            field,
            op: RangeOp::Between,
            value: RangeValue::U64 {
                lo: lo.min(hi),
                hi: Some(lo.max(hi)),
            },
        });
    }
    let (op, num_str) = if let Some(r) = s.strip_prefix(">=") {
        (RangeOp::Ge, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (RangeOp::Gt, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (RangeOp::Le, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (RangeOp::Lt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (RangeOp::Between, r)
    } else {
        (RangeOp::Between, s)
    };
    let val = num_str.trim().parse::<u64>().ok()?;
    let hi = matches!(op, RangeOp::Between).then_some(val);
    Some(RangeExpr {
        field,
        op,
        value: RangeValue::U64 { lo: val, hi },
    })
}

fn parse_size_range(spec: &str) -> Option<RangeExpr> {
    let s = spec.trim().to_ascii_lowercase();
    match s.as_str() {
        "empty" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Le,
                value: RangeValue::U64 { lo: 0, hi: None },
            });
        }
        "tiny" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Le,
                value: RangeValue::U64 {
                    lo: 10 * 1024,
                    hi: None,
                },
            });
        }
        "small" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Between,
                value: RangeValue::U64 {
                    lo: 10 * 1024,
                    hi: Some(1024 * 1024),
                },
            });
        }
        "medium" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Between,
                value: RangeValue::U64 {
                    lo: 1024 * 1024,
                    hi: Some(100 * 1024 * 1024),
                },
            });
        }
        "large" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Ge,
                value: RangeValue::U64 {
                    lo: 100 * 1024 * 1024,
                    hi: None,
                },
            });
        }
        "huge" | "giant" => {
            return Some(RangeExpr {
                field: FieldKind::Size,
                op: RangeOp::Ge,
                value: RangeValue::U64 {
                    lo: 1024 * 1024 * 1024,
                    hi: None,
                },
            });
        }
        _ => {}
    }

    if let Some((lo_s, hi_s)) = s.split_once("..") {
        let lo = parse_byte_size(lo_s.trim())?;
        let hi = parse_byte_size(hi_s.trim())?;
        return Some(RangeExpr {
            field: FieldKind::Size,
            op: RangeOp::Between,
            value: RangeValue::U64 {
                lo: lo.min(hi),
                hi: Some(lo.max(hi)),
            },
        });
    }

    let (op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (RangeOp::Ge, r)
    } else if let Some(r) = s.strip_prefix('>') {
        (RangeOp::Gt, r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (RangeOp::Le, r)
    } else if let Some(r) = s.strip_prefix('<') {
        (RangeOp::Lt, r)
    } else if let Some(r) = s.strip_prefix('=') {
        (RangeOp::Between, r)
    } else {
        (RangeOp::Between, s.as_str())
    };

    let bytes = parse_byte_size(rest.trim())?;
    let hi = matches!(op, RangeOp::Between).then_some(bytes);
    Some(RangeExpr {
        field: FieldKind::Size,
        op,
        value: RangeValue::U64 { lo: bytes, hi },
    })
}

fn parse_byte_size(raw: &str) -> Option<u64> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_part, unit_part) = s.split_at(split_idx);
    if num_part.is_empty() {
        return None;
    }
    let mult: u64 = match unit_part.trim() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => return None,
    };
    if num_part.contains('.') {
        let val = num_part.parse::<f64>().ok()?;
        if !val.is_finite() || val < 0.0 {
            return None;
        }
        Some((val * mult as f64).round() as u64)
    } else {
        let val = num_part.parse::<u64>().ok()?;
        val.checked_mul(mult)
    }
}

fn parse_date_range(field: FieldKind, spec: &str, now: i64) -> Option<RangeExpr> {
    let s = spec.trim().to_ascii_lowercase().replace(['_', ' '], "");
    if s.is_empty() {
        return None;
    }

    let start_of_today = now - now.rem_euclid(86_400);
    match s.as_str() {
        "today" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: start_of_today,
                    hi: None,
                },
            });
        }
        "yesterday" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Between,
                value: RangeValue::I64 {
                    lo: start_of_today - 86_400,
                    hi: Some(start_of_today - 1),
                },
            });
        }
        "thisweek" | "week" | "lastweek" | "pastweek" | "lastweeks" | "pastweeks" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: now - 7 * 86_400,
                    hi: None,
                },
            });
        }
        "thismonth" | "month" | "lastmonth" | "pastmonth" | "lastmonths" | "pastmonths" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: now - 30 * 86_400,
                    hi: None,
                },
            });
        }
        "thisyear" | "year" | "lastyear" | "pastyear" | "lastyears" | "pastyears" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: now - 365 * 86_400,
                    hi: None,
                },
            });
        }
        "lastday" | "pastday" | "thisday" => {
            return Some(RangeExpr {
                field,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: now - 86_400,
                    hi: None,
                },
            });
        }
        _ => {}
    }

    if let Some((lo_s, hi_s)) = s.split_once("..") {
        let (lo_start, _) = parse_date_endpoint(lo_s.trim(), now)?;
        let (_, hi_end) = parse_date_endpoint(hi_s.trim(), now)?;
        let (lo, hi) = if lo_start <= hi_end {
            (lo_start, hi_end)
        } else {
            (hi_end, lo_start)
        };
        return Some(RangeExpr {
            field,
            op: RangeOp::Between,
            value: RangeValue::I64 { lo, hi: Some(hi) },
        });
    }

    let (explicit_op, rest) = if let Some(r) = s.strip_prefix(">=") {
        (Some(RangeOp::Ge), r)
    } else if let Some(r) = s.strip_prefix('>') {
        (Some(RangeOp::Gt), r)
    } else if let Some(r) = s.strip_prefix("<=") {
        (Some(RangeOp::Le), r)
    } else if let Some(r) = s.strip_prefix('<') {
        (Some(RangeOp::Lt), r)
    } else if let Some(r) = s.strip_prefix('=') {
        (Some(RangeOp::Between), r)
    } else {
        (None, s.as_str())
    };

    // Relative duration (e.g. `7d`, `24h`, `2w`, `3mo`, `1y`, `last7d`, `past7d`, `-7d`).
    let rel_candidate = rest
        .strip_prefix("last")
        .or_else(|| rest.strip_prefix("past"))
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    if let Some(dur_secs) = parse_relative_duration_secs(rel_candidate) {
        let cutoff = now.saturating_sub(dur_secs);
        // For relative durations:
        // - `modified:7d`, `modified:<7d`, `modified:<=7d`, `modified:last7d` => within the last 7d (`>= cutoff`)
        // - `modified:>7d`, `modified:>=7d` => older than 7d (`<= cutoff`)
        let op = match explicit_op {
            None | Some(RangeOp::Lt) | Some(RangeOp::Le) | Some(RangeOp::Between) => RangeOp::Ge,
            Some(RangeOp::Gt) => RangeOp::Lt,
            Some(RangeOp::Ge) => RangeOp::Le,
        };
        return Some(RangeExpr {
            field,
            op,
            value: RangeValue::I64 {
                lo: cutoff,
                hi: None,
            },
        });
    }

    let (start_ts, end_ts) = parse_date_endpoint(rest, now)?;
    match explicit_op {
        None | Some(RangeOp::Between) => Some(RangeExpr {
            field,
            op: RangeOp::Between,
            value: RangeValue::I64 {
                lo: start_ts,
                hi: Some(end_ts),
            },
        }),
        Some(RangeOp::Ge) => Some(RangeExpr {
            field,
            op: RangeOp::Ge,
            value: RangeValue::I64 {
                lo: start_ts,
                hi: None,
            },
        }),
        Some(RangeOp::Gt) => Some(RangeExpr {
            field,
            op: RangeOp::Gt,
            value: RangeValue::I64 {
                lo: end_ts,
                hi: None,
            },
        }),
        Some(RangeOp::Le) => Some(RangeExpr {
            field,
            op: RangeOp::Le,
            value: RangeValue::I64 {
                lo: end_ts,
                hi: None,
            },
        }),
        Some(RangeOp::Lt) => Some(RangeExpr {
            field,
            op: RangeOp::Lt,
            value: RangeValue::I64 {
                lo: start_ts,
                hi: None,
            },
        }),
    }
}

fn parse_relative_duration_secs(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let split_idx = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if split_idx == 0 || split_idx == s.len() {
        return None;
    }
    let (num_str, unit_str) = s.split_at(split_idx);
    let count = num_str.parse::<i64>().ok()?;
    let unit_secs: i64 = match unit_str {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "wks" | "week" | "weeks" => 7 * 86_400,
        "mo" | "mon" | "month" | "months" => 30 * 86_400,
        "y" | "yr" | "yrs" | "year" | "years" => 365 * 86_400,
        _ => return None,
    };
    count.checked_mul(unit_secs)
}

fn parse_date_endpoint(s: &str, now: i64) -> Option<(i64, i64)> {
    let rel = s
        .strip_prefix("last")
        .or_else(|| s.strip_prefix("past"))
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    if let Some(dur) = parse_relative_duration_secs(rel) {
        let ts = now.saturating_sub(dur);
        return Some((ts, ts));
    }

    // Check `YYYY-MM-DD` or `YYYY/MM/DD`
    let normalized = s.replace('/', "-");
    let parts: Vec<&str> = normalized.split('-').collect();
    if parts.len() == 3 {
        let year = parts[0].parse::<i32>().ok()?;
        let month = parts[1].parse::<u32>().ok()?;
        let day = parts[2].parse::<u32>().ok()?;
        let start = civil_ymd_to_unix_secs(year, month, day)?;
        return Some((start, start + 86_399));
    }
    if parts.len() == 2 {
        let year = parts[0].parse::<i32>().ok()?;
        let month = parts[1].parse::<u32>().ok()?;
        let start = civil_ymd_to_unix_secs(year, month, 1)?;
        let last_day = days_in_month(year, month)?;
        let end = civil_ymd_to_unix_secs(year, month, last_day)? + 86_399;
        return Some((start, end));
    }
    if parts.len() == 1
        && s.len() == 4
        && let Ok(year) = s.parse::<i32>()
        && (1970..=2200).contains(&year)
    {
        let start = civil_ymd_to_unix_secs(year, 1, 1)?;
        let end = civil_ymd_to_unix_secs(year, 12, 31)? + 86_399;
        return Some((start, end));
    }

    let ts = s.parse::<i64>().ok()?;
    Some((ts, ts))
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn civil_ymd_to_unix_secs(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1970..=2500).contains(&year) {
        return None;
    }
    let max_day = days_in_month(year, month)?;
    if day == 0 || day > max_day {
        return None;
    }
    let y = year as i64 - if month <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn ser<T: Serialize>(value: &T) -> Vec<u8> {
        bincode::serialize(value).unwrap()
    }

    fn de<T: DeserializeOwned>(bytes: &[u8]) -> T {
        bincode::deserialize(bytes).unwrap()
    }

    #[test]
    fn default_is_empty_and_safe() {
        let req = SearchRequest::default();
        assert_eq!(req.id, Uuid::nil());
        assert_eq!(req.limit, 50);
        assert!(matches!(req.mode, SearchMode::Auto));
        assert_eq!(req.offset, 0);
        assert!(req.timeout.is_none());
        assert!(matches!(req.query, QueryExpr::And(ref items) if items.is_empty()));
    }

    #[test]
    fn bincode_roundtrip_query() {
        let q = QueryExpr::And(vec![
            QueryExpr::Term(TermExpr {
                field: Some(FieldKind::Name),
                value: "report".into(),
                modifier: TermModifier::Prefix,
            }),
            QueryExpr::Range(RangeExpr {
                field: FieldKind::Modified,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: 1_700_000_000,
                    hi: None,
                },
            }),
        ]);

        let req = SearchRequest {
            id: Uuid::new_v4(),
            query: q,
            limit: 20,
            mode: SearchMode::Hybrid,
            timeout: None,
            offset: 0,
        };

        let bytes = ser(&req);
        let back: SearchRequest = de(&bytes);
        assert_eq!(back.limit, 20);
        assert!(matches!(back.mode, SearchMode::Hybrid));
        assert_eq!(back.timeout, None);
        assert_eq!(back.offset, 0);
    }

    #[test]
    fn failed_search_and_unrelated_response_are_rejected_after_wire_decode() {
        let id = Uuid::new_v4();
        let mut response = SearchResponse {
            id,
            hits: Vec::new(),
            total: 0,
            truncated: true,
            took_ms: 7,
            served_by: Some("service-unavailable".into()),
        };
        let payload = framing::serialize_search_response(&mut response).unwrap();
        let message = framing::encode_message(framing::MessageKind::Search, &payload).unwrap();
        let frame = framing::encode_frame(&message).unwrap();
        let (message, remaining) = framing::decode_frame(&frame).unwrap();
        assert!(remaining.is_empty());
        let (kind, payload) = framing::decode_message(&message).unwrap();
        assert_eq!(kind, framing::MessageKind::Search);
        let mut decoded: SearchResponse = de(payload);
        let failure = decoded.validate_for(id).unwrap_err().to_string();
        assert!(failure.contains("could not complete this search"));

        // A healthy response for an older query must not replace the current
        // query's results just because the framing and message kind are valid.
        decoded.served_by = Some("service".into());
        let failure = decoded
            .validate_for(Uuid::new_v4())
            .unwrap_err()
            .to_string();
        assert!(failure.contains("does not match request"));
    }

    #[test]
    fn valid_empty_and_truncated_searches_are_not_misclassified_as_failures() {
        let id = Uuid::new_v4();
        for (total, truncated) in [(0, false), (42, true), (42, false)] {
            for served_by in [None, Some("service".to_string())] {
                let response = SearchResponse {
                    id,
                    hits: Vec::new(),
                    total,
                    truncated,
                    took_ms: 0,
                    served_by,
                };
                let decoded: SearchResponse = de(&ser(&response));
                decoded.validate_for(id).unwrap();
                assert_eq!(decoded.total, total);
                assert_eq!(decoded.truncated, truncated);
            }
        }
    }

    #[test]
    fn search_request_with_timeout_roundtrip() {
        let req = SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: None,
                value: "foo".into(),
                modifier: TermModifier::Term,
            }),
            limit: 5,
            mode: SearchMode::Auto,
            timeout: Some(Duration::from_millis(250)),
            offset: 7,
        };
        let bytes = ser(&req);
        let back: SearchRequest = de(&bytes);
        assert_eq!(back.timeout, Some(Duration::from_millis(250)));
        assert_eq!(back.offset, 7);
    }

    #[test]
    fn volume_status_fields_present() {
        let v = VolumeStatus {
            volume: 1,
            indexed_files: 10,
            indexed_bytes: 1024,
            pending_files: 2,
            pending_bytes: 512,
            last_usn: Some(42),
            journal_id: Some(7),
        };
        let encoded = ser(&v);
        let decoded: VolumeStatus = de(&encoded);
        assert_eq!(decoded.last_usn, Some(42));
        assert_eq!(decoded.journal_id, Some(7));
        assert_eq!(decoded.indexed_bytes, 1024);
        assert_eq!(decoded.pending_bytes, 512);
    }

    #[test]
    fn metrics_snapshot_serializes() {
        let m = MetricsSnapshot {
            search_latency_ms_p50: Some(12.3),
            search_latency_ms_p95: Some(45.6),
            worker_cpu_pct: Some(10.0),
            worker_mem_bytes: Some(1024),
            queue_depth: Some(5),
            active_workers: Some(2),
            content_enqueued: Some(9),
            content_dropped: Some(1),
        };
        let bytes = ser(&m);
        let back: MetricsSnapshot = de(&bytes);
        assert_eq!(back.queue_depth, Some(5));
        assert_eq!(back.active_workers, Some(2));
        assert_eq!(back.content_enqueued, Some(9));
        assert_eq!(back.content_dropped, Some(1));
    }

    #[test]
    fn search_request_default_is_reasonable() {
        let req = SearchRequest::default();
        assert_eq!(req.id, Uuid::nil());
        assert_eq!(req.limit, 50);
        assert!(matches!(req.mode, SearchMode::Auto));
        assert_eq!(req.timeout, None);
        assert_eq!(req.offset, 0);
        assert!(matches!(req.query, QueryExpr::And(ref items) if items.is_empty()));
    }

    #[test]
    fn request_builder_helpers_work() {
        let q = QueryExpr::Term(TermExpr {
            field: Some(FieldKind::Name),
            value: "foo".into(),
            modifier: TermModifier::Prefix,
        });
        let req = SearchRequest::with_query(q.clone())
            .with_timeout_ms(500)
            .with_limit(10)
            .with_offset(5)
            .with_mode(SearchMode::Content);

        assert_eq!(req.query, q);
        assert_eq!(req.timeout, Some(Duration::from_millis(500)));
        assert_eq!(req.limit, 10);
        assert_eq!(req.offset, 5);
        assert!(matches!(req.mode, SearchMode::Content));
    }

    #[test]
    fn unified_query_parser_handles_fields_booleans_phrases_prefixes_fuzzy_and_ranges() {
        // 2024-05-15 12:00:00 UTC = 19858 * 86400 + 43200 = 1_715_774_400
        let now = 1_715_774_400_i64;
        let start_of_today = 1_715_731_200_i64;

        let parsed = parse_user_query(
            r#"ext:.rs,toml kind:code size:>10mb modified:today -path:"target/debug" (report* OR budget~1)"#,
            now,
        );

        assert_eq!(
            parsed,
            QueryExpr::And(vec![
                QueryExpr::Or(vec![
                    QueryExpr::Term(TermExpr {
                        field: Some(FieldKind::Ext),
                        value: "rs".into(),
                        modifier: TermModifier::Term,
                    }),
                    QueryExpr::Term(TermExpr {
                        field: Some(FieldKind::Ext),
                        value: "toml".into(),
                        modifier: TermModifier::Term,
                    }),
                ]),
                QueryExpr::Term(TermExpr {
                    field: Some(FieldKind::Kind),
                    value: "code".into(),
                    modifier: TermModifier::Term,
                }),
                QueryExpr::Range(RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Gt,
                    value: RangeValue::U64 {
                        lo: 10 * 1024 * 1024,
                        hi: None,
                    },
                }),
                QueryExpr::Range(RangeExpr {
                    field: FieldKind::Modified,
                    op: RangeOp::Ge,
                    value: RangeValue::I64 {
                        lo: start_of_today,
                        hi: None,
                    },
                }),
                QueryExpr::Not(Box::new(QueryExpr::Term(TermExpr {
                    field: Some(FieldKind::Path),
                    value: "target/debug".into(),
                    modifier: TermModifier::Phrase,
                }))),
                QueryExpr::Or(vec![
                    QueryExpr::Term(TermExpr {
                        field: None,
                        value: "report".into(),
                        modifier: TermModifier::Prefix,
                    }),
                    QueryExpr::Term(TermExpr {
                        field: None,
                        value: "budget".into(),
                        modifier: TermModifier::Fuzzy(1),
                    }),
                ]),
            ])
        );
    }

    #[test]
    fn natural_language_and_iso_dates_parse_accurately() {
        let now = 1_715_774_400_i64; // 2024-05-15 12:00:00 UTC
        let start_of_today = 1_715_731_200_i64;

        assert_eq!(
            parse_user_query("modified:yesterday", now),
            QueryExpr::Range(RangeExpr {
                field: FieldKind::Modified,
                op: RangeOp::Between,
                value: RangeValue::I64 {
                    lo: start_of_today - 86_400,
                    hi: Some(start_of_today - 1),
                },
            })
        );

        assert_eq!(
            parse_user_query("date:last week", now),
            QueryExpr::Range(RangeExpr {
                field: FieldKind::Modified,
                op: RangeOp::Ge,
                value: RangeValue::I64 {
                    lo: now - 7 * 86_400,
                    hi: None,
                },
            })
        );

        // Calendar year 2024 (leap year: 366 days)
        let y2024_start = civil_ymd_to_unix_secs(2024, 1, 1).unwrap();
        let y2024_end = civil_ymd_to_unix_secs(2024, 12, 31).unwrap() + 86_399;
        assert_eq!(
            parse_user_query("created:2024", now),
            QueryExpr::Range(RangeExpr {
                field: FieldKind::Created,
                op: RangeOp::Between,
                value: RangeValue::I64 {
                    lo: y2024_start,
                    hi: Some(y2024_end),
                },
            })
        );

        // Explicit ISO date range
        let d_lo = civil_ymd_to_unix_secs(2024, 5, 1).unwrap();
        let d_hi = civil_ymd_to_unix_secs(2024, 5, 10).unwrap() + 86_399;
        assert_eq!(
            parse_user_query("modified:2024-05-01..2024-05-10", now),
            QueryExpr::Range(RangeExpr {
                field: FieldKind::Modified,
                op: RangeOp::Between,
                value: RangeValue::I64 {
                    lo: d_lo,
                    hi: Some(d_hi),
                },
            })
        );
    }
}
