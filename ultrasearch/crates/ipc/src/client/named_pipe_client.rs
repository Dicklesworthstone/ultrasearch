#![cfg(target_os = "windows")]

use crate::{
    ReloadConfigRequest, ReloadConfigResponse, SearchRequest, SearchResponse, StatusRequest,
    StatusResponse, framing,
};
use anyhow::{Result, bail};
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

/// Expose reconnect success count for telemetry/testing.
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
        self.request(&req).await
    }

    pub async fn search(&self, req: SearchRequest) -> Result<SearchResponse> {
        self.request(&req).await
    }

    pub async fn reload_config(&self, req: ReloadConfigRequest) -> Result<ReloadConfigResponse> {
        self.request(&req).await
    }

    async fn request<Req, Resp>(&self, req: &Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        // Serialize payload
        let payload = bincode::serialize(req)?;
        // Frame it (adds length header)
        let framed = framing::encode_frame(&payload)?;

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

                if resp_len == 0 || resp_len > MAX_MESSAGE_BYTES {
                    bail!("invalid response length {}", resp_len);
                }

                // Read response body
                let mut buf = vec![0u8; resp_len];
                conn.read_exact(&mut buf).await?;

                // Deserialize directly from the body buffer
                // (framing::decode_frame expects [header + body], but we already consumed header.
                // Since we trust the stream logic here, we can skip using decode_frame logic for the buffer check
                // and just deserialize the body.)
                let resp: Resp = bincode::deserialize(&buf)?;
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
