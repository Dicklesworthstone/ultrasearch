use anyhow::Result;
use ipc::{
    ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse, SearchRequest,
    SearchResponse, StatusRequest, StatusResponse,
};
#[cfg(windows)]
use std::sync::Arc;
#[cfg(any(windows, test))]
use std::time::Duration;

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
            self.inner
                .as_ref()
                .clone()
                .with_request_timeout(search_transport_timeout(req.timeout))
                .search(req)
                .await
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
            self.inner
                .as_ref()
                .clone()
                .with_request_timeout(Duration::from_secs(2))
                .status(req)
                .await
        }
        #[cfg(not(windows))]
        {
            Ok(StatusResponse {
                id: req.id,
                volumes: vec![],
                last_index_commit_ts: None,
                scheduler_state: "ui-stub".into(),
                content_jobs_total: Some(0),
                content_jobs_remaining: Some(0),
                content_bytes_total: Some(0),
                content_bytes_remaining: Some(0),
                metrics: None,
                served_by: Some("ui-stub".into()),
            })
        }
    }

    pub async fn rescan(&self, req: RescanRequest) -> Result<RescanResponse> {
        #[cfg(windows)]
        {
            self.inner.rescan(req).await
        }
        #[cfg(not(windows))]
        {
            Ok(RescanResponse {
                id: req.id,
                success: true,
                message: Some("ui-stub".into()),
            })
        }
    }

    pub async fn reload_config(&self, req: ReloadConfigRequest) -> Result<ReloadConfigResponse> {
        #[cfg(windows)]
        {
            self.inner.reload_config(req).await
        }
        #[cfg(not(windows))]
        {
            Ok(ReloadConfigResponse {
                id: req.id,
                success: true,
                message: Some("ui-stub".into()),
            })
        }
    }
}

#[cfg(any(windows, test))]
fn search_transport_timeout(query_timeout: Option<Duration>) -> Duration {
    // A cold filename/content search must get its requested execution budget,
    // plus connection/framing time. The PipeClient default is only a 750 ms probe.
    query_timeout
        .unwrap_or(Duration::from_secs(5))
        .saturating_add(Duration::from_secs(1))
}

impl Default for IpcClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_transport_budget_covers_execution_and_io() {
        assert_eq!(search_transport_timeout(None), Duration::from_secs(6));
        assert_eq!(
            search_transport_timeout(Some(Duration::from_secs(5))),
            Duration::from_secs(6)
        );
        assert_eq!(
            search_transport_timeout(Some(Duration::from_secs(10))),
            Duration::from_secs(11)
        );
        assert_eq!(search_transport_timeout(Some(Duration::MAX)), Duration::MAX);
    }

    /// Uses the same UI adapter and real Windows named pipes as the application.
    /// The delayed reply deliberately exceeds the old 750 ms client probe budget.
    #[cfg(windows)]
    #[tokio::test]
    async fn native_ui_search_accepts_a_cold_reply_after_750ms() -> Result<()> {
        use ipc::framing::{self, MessageKind};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;
        use uuid::Uuid;

        let name = format!(r"\\.\pipe\ultrasearch-ui-cold-{}", Uuid::new_v4());
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        let client = IpcClient {
            inner: Arc::new(PipeClient::new(&name)),
        };
        let id = Uuid::new_v4();
        let peer = tokio::spawn(async move {
            server.connect().await?;
            let mut server = server;
            let mut length = [0; 4];
            server.read_exact(&mut length).await?;
            let length = u32::from_le_bytes(length) as usize;
            anyhow::ensure!(length <= framing::MAX_FRAME, "oversized fixture request");
            let mut message = vec![0; length];
            server.read_exact(&mut message).await?;
            let (kind, payload) = framing::decode_message(&message)?;
            anyhow::ensure!(kind == MessageKind::Search, "expected search");
            let request: SearchRequest = bincode::deserialize(payload)?;
            anyhow::ensure!(request.id == id, "wrong request ID");
            tokio::time::sleep(Duration::from_millis(1_000)).await;
            let reply = SearchResponse {
                id,
                hits: vec![ipc::SearchHit {
                    key: core_types::DocKey::from_parts(1, 42),
                    score: 1.0,
                    name: Some("test-cold-start.txt".into()),
                    path: Some(r"C:\fixtures\test-cold-start.txt".into()),
                    ext: Some("txt".into()),
                    size: Some(20),
                    modified: Some(1),
                    snippet: None,
                }],
                total: 1,
                truncated: false,
                took_ms: 1_000,
                served_by: Some("native-ui-test".into()),
            };
            server
                .write_all(&framing::encode_frame(&framing::encode_message(
                    MessageKind::Search,
                    &bincode::serialize(&reply)?,
                )?)?)
                .await?;
            // Keep the pipe alive until the client has consumed the response.
            let mut end = [0];
            server.read(&mut end).await?;
            Ok::<_, anyhow::Error>(())
        });
        let response = tokio::time::timeout(Duration::from_secs(10), async {
            let response = client
                .search(SearchRequest {
                    id,
                    query: ipc::parse_query_str("test"),
                    timeout: Some(Duration::from_secs(5)),
                    ..SearchRequest::default()
                })
                .await;
            peer.await??;
            response
        })
        .await??;
        assert_eq!(response.total, 1);
        assert_eq!(
            response.hits[0].name.as_deref(),
            Some("test-cold-start.txt")
        );
        Ok(())
    }
}
