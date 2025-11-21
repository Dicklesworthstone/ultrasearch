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

/// Fields that can be targeted explicitly in the query language.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermExpr {
    pub field: Option<FieldKind>, // None => default (name + content)
    pub value: String,
    pub modifier: TermModifier,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
    Between,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RangeValue {
    I64 { lo: i64, hi: Option<i64> }, // timestamps
    U64 { lo: u64, hi: Option<u64> }, // sizes
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeExpr {
    pub field: FieldKind,
    pub op: RangeOp,
    pub value: RangeValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SearchMode {
    Auto,        // planner decides
    NameOnly,    // metadata index only
    Content,     // content index
    Hybrid,      // meta + content merge
}

pub mod framing;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub id: Uuid,
    pub query: QueryExpr,
    pub limit: u32,
    pub mode: SearchMode,
    #[serde(default)]
    pub timeout: Option<Duration>,
    #[serde(default)]
    pub offset: u32,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRequest {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub volume: u16,
    pub indexed_files: u64,
    pub pending_files: u64,
    pub last_usn: Option<u64>,
    pub journal_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub id: Uuid,
    pub volumes: Vec<VolumeStatus>,
    pub last_index_commit_ts: Option<i64>,
    pub scheduler_state: String,
    pub metrics: Option<MetricsSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub search_latency_ms_p50: Option<f64>,
    pub search_latency_ms_p95: Option<f64>,
    pub worker_cpu_pct: Option<f64>,
    pub worker_mem_bytes: Option<u64>,
    pub queue_depth: Option<u64>,
    pub active_workers: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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

        let bytes = bincode::serialize(&req).expect("serialize");
        let back: SearchRequest = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.limit, 20);
        assert_eq!(matches!(back.mode, SearchMode::Hybrid), true);
        assert_eq!(back.timeout, None);
        assert_eq!(back.offset, 0);
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
        let bytes = bincode::serialize(&req).unwrap();
        let back: SearchRequest = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.timeout, Some(Duration::from_millis(250)));
        assert_eq!(back.offset, 7);
    }

    #[test]
    fn volume_status_fields_present() {
        let v = VolumeStatus {
            volume: 1,
            indexed_files: 10,
            pending_files: 2,
            last_usn: Some(42),
            journal_id: Some(7),
        };
        let encoded = bincode::serialize(&v).unwrap();
        let decoded: VolumeStatus = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.last_usn, Some(42));
        assert_eq!(decoded.journal_id, Some(7));
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
        };
        let bytes = bincode::serialize(&m).unwrap();
        let back: MetricsSnapshot = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.queue_depth, Some(5));
        assert_eq!(back.active_workers, Some(2));
    }
}
}
