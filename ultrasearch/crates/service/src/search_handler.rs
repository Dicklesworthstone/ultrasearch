use ipc::{SearchRequest, SearchResponse};
use std::sync::{Arc, OnceLock};

/// Trait for executing search requests coming in via IPC.
pub trait SearchHandler: Send + Sync {
    fn search(&self, req: SearchRequest) -> SearchResponse;
}

/// Default stub handler that returns an empty result set.
#[derive(Debug)]
pub struct StubSearchHandler;

impl SearchHandler for StubSearchHandler {
    fn search(&self, req: SearchRequest) -> SearchResponse {
        SearchResponse {
            id: req.id,
            hits: Vec::new(),
            total: 0,
            truncated: false,
            took_ms: 0,
            served_by: Some("stub-handler".into()),
        }
    }
}

static HANDLER: OnceLock<Arc<dyn SearchHandler>> = OnceLock::new();

pub fn set_search_handler(handler: Arc<dyn SearchHandler>) {
    let _ = HANDLER.set(handler);
}

pub fn search(req: SearchRequest) -> SearchResponse {
    if let Some(h) = HANDLER.get() {
        h.search(req)
    } else {
        StubSearchHandler.search(req)
    }
}
