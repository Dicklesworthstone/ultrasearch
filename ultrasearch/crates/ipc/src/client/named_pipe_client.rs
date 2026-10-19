#![cfg(target_os = "windows")]

use crate::{
    ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse, SearchRequest,
    SearchResponse, StatusRequest, StatusResponse, framing,
};
use anyhow::{Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{Duration, sleep};
use tracing::warn;

const DEFAULT_PIPE_NAME: &str = r#"\\.\pipe\ultrasearch"#;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 750;
const DEFAULT_RETRIES: u32 = 5;
const DEFAULT_BACKOFF_MS: u64 = 100;

static RECONNECT_SUCCESSES: OnceLock<AtomicUsize> = OnceLock::new();

fn reconnect_counter() -> &'static AtomicUsize {
    RECONNECT_SUCCESSES.get_or_init(|| AtomicUsize::new(0))
}

/// Expose reconnect success count for diagnostics/testing.
#[allow(dead_code)]
pub fn reconnect_success_count() -> usize {
    reconnect_counter().load(Ordering::Relaxed)
}

/// Named-pipe IPC client for UltraSearch.
#[derive(Debug, Clone)]
pub struct PipeClient {
    pipe_name: String,
    request_timeout: Duration,
    retries: u32,
    backoff: Duration,
}

impl Default for PipeClient {
    fn default() -> Self {
        Self {
            pipe_name: DEFAULT_PIPE_NAME.to_string(),
            request_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            retries: DEFAULT_RETRIES,
            backoff: Duration::from_millis(DEFAULT_BACKOFF_MS),
        }
    }
}

impl PipeClient {
    pub fn new(pipe_name: impl Into<String>) -> Self {
        Self {
            pipe_name: pipe_name.into(),
            request_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            retries: DEFAULT_RETRIES,
            backoff: Duration::from_millis(DEFAULT_BACKOFF_MS),
        }
    }

    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    pub async fn status(&self, req: StatusRequest) -> Result<StatusResponse> {
        self.request(framing::MessageKind::Status, &req).await
    }

    pub async fn search(&self, req: SearchRequest) -> Result<SearchResponse> {
        let response: SearchResponse = self.request(framing::MessageKind::Search, &req).await?;
        // Transport success does not establish successful query execution.
        // Validate outside the transport retry loop so an explicit service
        // failure does not replay an expensive query on every reconnect turn.
        response.validate_for(req.id)?;
        Ok(response)
    }

    pub async fn reload_config(&self, req: ReloadConfigRequest) -> Result<ReloadConfigResponse> {
        self.request(framing::MessageKind::ReloadConfig, &req).await
    }

    pub async fn rescan(&self, req: RescanRequest) -> Result<RescanResponse> {
        self.request(framing::MessageKind::Rescan, &req).await
    }

    /// Measure round-trip IPC liveness using the fast-path `MessageKind::Ping` echo.
    pub async fn ping(&self) -> Result<Duration> {
        let id = uuid::Uuid::new_v4();
        let mut payload = Vec::with_capacity(20);
        payload.extend_from_slice(b"PING");
        payload.extend_from_slice(id.as_bytes());
        let framed = framing::encode_frame(&framing::encode_message(
            framing::MessageKind::Ping,
            &payload,
        )?)?;

        let started = std::time::Instant::now();
        let fut = async move {
            let mut conn = ClientOptions::new().open(&self.pipe_name)?;
            conn.write_all(&framed).await?;
            let mut len_buf = [0u8; 4];
            conn.read_exact(&mut len_buf).await?;
            let resp_len = u32::from_le_bytes(len_buf) as usize;
            ensure!(
                resp_len > 0 && resp_len <= MAX_MESSAGE_BYTES,
                "invalid ping response length {resp_len}"
            );
            let mut buf = vec![0u8; resp_len];
            conn.read_exact(&mut buf).await?;
            let (response_kind, echoed) = framing::decode_message(&buf)?;
            ensure!(
                response_kind == framing::MessageKind::Ping,
                "unexpected ping response kind {response_kind:?}"
            );
            ensure!(
                echoed == id.as_bytes().as_slice(),
                "mismatched ping echo UUID"
            );
            Ok::<Duration, anyhow::Error>(started.elapsed())
        };

        tokio::time::timeout(self.request_timeout, fut)
            .await
            .map_err(anyhow::Error::from)?
    }

    async fn request<Req, Resp>(&self, kind: framing::MessageKind, req: &Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        // Serialize payload
        let payload = bincode::serialize(req)?;
        // Enforce the identity layout version before either peer parses bincode.
        let framed = framing::encode_frame(&framing::encode_message(kind, &payload)?)?;

        let mut attempt = 0;
        let mut last_err: Option<anyhow::Error> = None;

        while attempt <= self.retries {
            let was_retry = attempt > 0;
            let frame = framed.clone();
            let fut = async move {
                // Return anyhow::Result to simplify error handling.
                // Connect (new pipe each attempt)
                let mut conn = match ClientOptions::new().open(&self.pipe_name) {
                    Ok(c) => c,
                    Err(e) => return Err(anyhow::Error::new(e)),
                };

                // Write the framed request
                conn.write_all(&frame).await?;

                // Read response header
                let mut len_buf = [0u8; 4];
                conn.read_exact(&mut len_buf).await?;
                let resp_len = u32::from_le_bytes(len_buf) as usize;

                if resp_len == 0 {
                    bail!(
                        "incompatible UltraSearch IPC protocol: empty response; update UI and service together"
                    );
                }
                if resp_len > MAX_MESSAGE_BYTES {
                    bail!("invalid response length {}", resp_len);
                }

                // Read response body
                let mut buf = vec![0u8; resp_len];
                conn.read_exact(&mut buf).await?;

                let (response_kind, payload) = framing::decode_message(&buf)?;
                ensure!(
                    response_kind == kind,
                    "unexpected IPC response kind {response_kind:?}; expected {kind:?}"
                );
                let mut cursor = std::io::Cursor::new(payload);
                let resp: Resp = bincode::deserialize_from(&mut cursor)?;
                ensure!(
                    cursor.position() as usize == payload.len(),
                    "IPC response has trailing bytes"
                );
                Ok(resp)
            };

            match tokio::time::timeout(self.request_timeout, fut).await {
                Ok(Ok(resp)) => {
                    if was_retry {
                        reconnect_counter().fetch_add(1, Ordering::Relaxed);
                    }
                    return Ok(resp);
                }
                Ok(Err(e)) => {
                    // Common reconnect cases: pipe missing (service down) or busy.
                    if let Some(code) = e
                        .downcast_ref::<std::io::Error>()
                        .and_then(|ioe| ioe.raw_os_error())
                    {
                        if code == 2 || code == 231 {
                            // 2 = ERROR_FILE_NOT_FOUND (service not up yet)
                            // 231 = ERROR_PIPE_BUSY (connecting during service restart)
                            warn!(
                                "pipe request attempt {}: service unavailable/busy (os err {})",
                                attempt + 1,
                                code
                            );
                            last_err = Some(e);
                            // Continue loop with backoff.
                        } else {
                            warn!("pipe request attempt {} failed: {e:?}", attempt + 1);
                            last_err = Some(e);
                        }
                    } else {
                        warn!("pipe request attempt {} failed: {e:?}", attempt + 1);
                        last_err = Some(e);
                    }
                }
                Err(e) => {
                    warn!("pipe request attempt {} timed out: {e:?}", attempt + 1);
                    let err: anyhow::Error = e.into();
                    last_err = Some(err);
                }
            }

            attempt += 1;
            if attempt <= self.retries {
                // Apply linear backoff; bump to exponential if needed.
                sleep(self.backoff * attempt.min(10)).await;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!(
                "request failed after {} attempts to {}",
                self.retries + 1,
                self.pipe_name
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use tokio::net::windows::named_pipe::ServerOptions;
    use uuid::Uuid;

    /// Exercise the real client, versioned framing, and a local native pipe.
    /// The outer deadline also bounds a server waiting after client failure.
    async fn exchange_search_response(
        request_id: Uuid,
        mut response: SearchResponse,
    ) -> Result<Result<SearchResponse>> {
        let pipe_name = format!(r"\\.\pipe\ultrasearch-search-validation-{}", Uuid::new_v4());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)?;
        let client = PipeClient::new(pipe_name)
            .with_request_timeout(Duration::from_secs(2))
            .with_retries(0);
        let request = SearchRequest {
            id: request_id,
            ..SearchRequest::default()
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let serve = async {
                server.connect().await?;
                let mut length = [0; 4];
                server.read_exact(&mut length).await?;
                let length = u32::from_le_bytes(length) as usize;
                ensure!(length <= framing::MAX_FRAME, "oversized test request");
                let mut message = vec![0; length];
                server.read_exact(&mut message).await?;
                let (kind, payload) = framing::decode_message(&message)?;
                ensure!(kind == framing::MessageKind::Search, "expected search");
                let decoded: SearchRequest = bincode::deserialize(payload)?;
                ensure!(decoded.id == request_id, "incorrect request identity");
                let payload = framing::serialize_search_response(&mut response)?;
                let message = framing::encode_message(kind, &payload)?;
                server.write_all(&framing::encode_frame(&message)?).await?;
                Ok::<(), anyhow::Error>(())
            };
            let (served, result) = tokio::join!(serve, client.search(request));
            served?;
            Ok(result)
        })
        .await
        .context("native named-pipe search exchange timed out")?
    }

    #[tokio::test]
    async fn native_pipe_rejects_failed_and_mismatched_search_responses() -> Result<()> {
        let request_id = Uuid::new_v4();
        for (response_id, served_by, expected_error) in [
            (request_id, "service-unavailable", "could not complete"),
            (Uuid::new_v4(), "service", "does not match request"),
        ] {
            let response = SearchResponse {
                id: response_id,
                hits: Vec::new(),
                total: 0,
                truncated: true,
                took_ms: 0,
                served_by: Some(served_by.into()),
            };
            let error = exchange_search_response(request_id, response)
                .await?
                .unwrap_err();
            ensure!(error.to_string().contains(expected_error), "{error:#}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn native_pipe_accepts_empty_and_bounded_search_responses() -> Result<()> {
        let request_id = Uuid::new_v4();
        for (total, truncated) in [(0, false), (100_000, true)] {
            let response = SearchResponse {
                id: request_id,
                hits: Vec::new(),
                total,
                truncated,
                took_ms: 3,
                served_by: Some("service".into()),
            };
            let decoded = exchange_search_response(request_id, response).await??;
            assert_eq!(decoded.id, request_id);
            assert_eq!(decoded.total, total);
            assert_eq!(decoded.truncated, truncated);
            assert!(decoded.hits.is_empty());
        }
        Ok(())
    }
}
