#![cfg(target_os = "windows")]

use crate::{
    ReloadConfigRequest, ReloadConfigResponse, SearchRequest, SearchResponse, StatusRequest,
    StatusResponse, framing,
};
use anyhow::{Context, Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{Duration, sleep};
use tracing::warn;

const DEFAULT_PIPE_NAME: &str = r#"\\.\pipe\ultrasearch"#;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 750;
const DEFAULT_RETRIES: u32 = 2;
const DEFAULT_BACKOFF_MS: u64 = 50;

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
            let fut = async {
                let mut conn = ClientOptions::new()
                    .open(&self.pipe_name)
                    .with_context(|| format!("connect to pipe {}", self.pipe_name))?;

                // Write the framed request
                conn.write_all(&framed).await?;

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
                Ok(Ok(resp)) => return Ok(resp),
                Ok(Err(e)) => {
                    warn!("pipe request attempt {} failed: {e:?}", attempt + 1);
                    last_err = Some(e);
                }
                Err(e) => {
                    warn!("pipe request attempt {} timed out: {e:?}", attempt + 1);
                    let err: anyhow::Error = e.into();
                    last_err = Some(err);
                }
            }

            attempt += 1;
            if attempt <= self.retries {
                sleep(self.backoff * attempt.min(10)).await;
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("request failed")))
    }
}
