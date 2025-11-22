use anyhow::Result;
use ipc::{SearchRequest, SearchResponse, StatusRequest, StatusResponse};
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use ipc::client::PipeClient;

/// A handle to the IPC client that can be used from GPUI models.
#[derive(Clone)]
pub struct IpcClient {
    #[cfg(windows)]
    inner: Arc<PipeClient>,
    // Stub state for non-Windows
    #[cfg(not(windows))]
    _stub: (),
}

impl IpcClient {
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            Self {
                inner: Arc::new(PipeClient::default()),
            }
        }
        #[cfg(not(windows))]
        {
            Self { _stub: () }
        }
    }

    pub async fn search(&self, req: SearchRequest) -> Result<SearchResponse> {
        #[cfg(windows)]
        {
            self.inner.search(req).await
        }
        #[cfg(not(windows))]
        {
            // Stub
            Ok(SearchResponse {
                id: req.id,
                hits: vec![],
                total: 0,
                truncated: false,
                took_ms: 0,
                served_by: Some("ui-stub".into()),
            })
        }
    }

    pub async fn status(&self, req: StatusRequest) -> Result<StatusResponse> {
        #[cfg(windows)]
        {
            self.inner.status(req).await
        }
        #[cfg(not(windows))]
        {
            Ok(StatusResponse {
                id: req.id,
                volumes: vec![],
                last_index_commit_ts: None,
                scheduler_state: "ui-stub".into(),
                metrics: None,
                served_by: Some("ui-stub".into()),
            })
        }
    }
}

impl Default for IpcClient {
    fn default() -> Self {
        Self::new()
    }
}
