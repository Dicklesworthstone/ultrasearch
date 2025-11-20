//! IPC protocol models for UltraSearch.
//!
//! These types are serialized with bincode over a length-prefixed pipe
//! framing (handled in the service/CLI/UI). The goal here is to model the
//! query AST, requests, and responses in a way that matches the architecture
//! plan without pulling in search/index dependencies.

use core_types::DocKey;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SearchMode {
    Auto,        // planner decides
    NameOnly,    // metadata index only
    Content,     // content index
    Hybrid,      // meta + content merge
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub id: Uuid,
    pub query: QueryExpr,
    pub limit: u32,
    pub mode: SearchMode,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub id: Uuid,
    pub volumes: Vec<VolumeStatus>,
    pub scheduler_state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };

        let bytes = bincode::serialize(&req).expect("serialize");
        let back: SearchRequest = bincode::deserialize(&bytes).expect("deserialize");
        assert_eq!(back.limit, 20);
        assert_eq!(matches!(back.mode, SearchMode::Hybrid), true);
    }
}
