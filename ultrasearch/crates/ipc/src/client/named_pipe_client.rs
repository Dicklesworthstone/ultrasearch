#![cfg(any(windows, test))]

use crate::framing;
#[cfg(windows)]
use crate::{
    ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse, SearchRequest,
    SearchResponse, StatusRequest, StatusResponse,
};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::time::{Duration, Instant, sleep_until, timeout_at};
use tracing::warn;
use uuid::Uuid;

const DEFAULT_PIPE_NAME: &str = r#"\\.\pipe\ultrasearch"#;
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

/// Named-pipe IPC client for UltraSearch. Reconnect only before sending a
/// request: a lost reply cannot establish whether the service applied a change.
#[derive(Debug, Clone)]
pub struct PipeClient {
    pipe_name: String,
    request_timeout: Duration,
    retries: u32,
    backoff: Duration,
}

impl Default for PipeClient {
    fn default() -> Self {
        Self::new(DEFAULT_PIPE_NAME)
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

    /// Total budget for connection attempts, backoff, write, and response read.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Additional connection attempts on missing/busy pipes, never request replays.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }

    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    #[cfg(windows)]
    pub async fn status(&self, req: StatusRequest) -> Result<StatusResponse> {
        let response: StatusResponse = self.request(framing::MessageKind::Status, &req).await?;
        validate_response_id(req.id, response.id)?;
        Ok(response)
    }

    #[cfg(windows)]
    pub async fn search(&self, req: SearchRequest) -> Result<SearchResponse> {
        let response: SearchResponse = self.request(framing::MessageKind::Search, &req).await?;
        response.validate_for(req.id)?;
        Ok(response)
    }

    #[cfg(windows)]
    pub async fn reload_config(&self, req: ReloadConfigRequest) -> Result<ReloadConfigResponse> {
        let response: ReloadConfigResponse = self
            .request(framing::MessageKind::ReloadConfig, &req)
            .await?;
        validate_control_response(
            req.id,
            response.id,
            response.success,
            response.message.as_deref(),
        )?;
        Ok(response)
    }

    #[cfg(windows)]
    pub async fn rescan(&self, req: RescanRequest) -> Result<RescanResponse> {
        let response: RescanResponse = self.request(framing::MessageKind::Rescan, &req).await?;
        validate_control_response(
            req.id,
            response.id,
            response.success,
            response.message.as_deref(),
        )?;
        Ok(response)
    }

    /// Measure round-trip IPC liveness. The body is raw `PING` plus 16 UUID
    /// bytes, matching the service fast-path, not a bincode request. Missing
    /// or busy pipes may be retried. A ping that was written is never replayed.
    #[cfg(windows)]
    pub async fn ping(&self) -> Result<Duration> {
        ensure!(
            !self.pipe_name.contains('\0'),
            "IPC pipe name contains a NUL"
        );
        let id = Uuid::new_v4();
        let mut body = Vec::with_capacity(20);
        body.extend_from_slice(b"PING");
        body.extend_from_slice(id.as_bytes());
        let framed =
            framing::encode_frame(&framing::encode_message(framing::MessageKind::Ping, &body)?)?;
        let started = Instant::now();
        self.transact_framed(
            &framed,
            || ClientOptions::new().open(&self.pipe_name),
            |kind, payload| {
                ensure!(
                    kind == framing::MessageKind::Ping,
                    "unexpected ping response kind {kind:?}"
                );
                ensure!(
                    payload.as_slice() == id.as_bytes(),
                    "mismatched ping echo UUID"
                );
                Ok(())
            },
        )
        .await?;
        Ok(started.elapsed())
    }

    #[cfg(windows)]
    async fn request<Req, Resp>(&self, kind: framing::MessageKind, req: &Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        ensure!(
            !self.pipe_name.contains('\0'),
            "IPC pipe name contains a NUL"
        );
        self.request_with(kind, req, || ClientOptions::new().open(&self.pipe_name))
            .await
    }

    /// The injected open operation is also used by deterministic stream tests.
    /// Once it succeeds, every failure is terminal for this invocation. Even a
    /// partial write might have delivered the complete request to another peer.
    async fn request_with<Req, Resp, C>(
        &self,
        kind: framing::MessageKind,
        req: &Req,
        open: impl FnMut() -> std::io::Result<C>,
    ) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
        C: AsyncRead + AsyncWrite + Unpin,
    {
        let payload = bincode::serialize(req)?;
        let framed = framing::encode_frame(&framing::encode_message(kind, &payload)?)?;
        self.transact_framed(&framed, open, |response_kind, payload| {
            ensure!(
                response_kind == kind,
                "unexpected IPC response kind {response_kind:?}; expected {kind:?}"
            );
            let payload_len = payload.len();
            let mut cursor = std::io::Cursor::new(payload);
            let response = bincode::deserialize_from(&mut cursor)?;
            ensure!(
                cursor.position() as usize == payload_len,
                "IPC response has trailing bytes"
            );
            Ok(response)
        })
        .await
    }

    /// Connect attempts may retry. The framed bytes are written at most once.
    async fn transact_framed<C, T>(
        &self,
        framed: &[u8],
        mut open: impl FnMut() -> std::io::Result<C>,
        decode: impl Fn(framing::MessageKind, Vec<u8>) -> Result<T>,
    ) -> Result<T>
    where
        C: AsyncRead + AsyncWrite + Unpin,
    {
        let deadline = Instant::now()
            .checked_add(self.request_timeout)
            .context("IPC request timeout is too large")?;
        ensure!(
            Instant::now() < deadline,
            "IPC deadline expired before connecting; request not sent"
        );
        let mut last_connect_error = None;
        // u64 accounting also handles with_retries(u32::MAX) without wrapping
        // the attempt counter or the diagnostic count.
        for attempt in 0..=u64::from(self.retries) {
            if Instant::now() >= deadline {
                if let Some(error) = last_connect_error {
                    return Err(self.connection_error(error, attempt, true));
                }
                anyhow::bail!("IPC deadline expired before connecting; request not sent");
            }
            let mut conn = match open() {
                Ok(conn) => conn,
                Err(error)
                    if matches!(error.raw_os_error(), Some(2) | Some(231))
                        && attempt < u64::from(self.retries) =>
                {
                    // ERROR_FILE_NOT_FOUND / ERROR_PIPE_BUSY: no connection
                    // exists, so no request byte could have been delivered.
                    warn!(
                        attempt = attempt + 1,
                        %error,
                        "IPC pipe unavailable; retrying connection only"
                    );
                    let delay = self.backoff.saturating_mul((attempt + 1).min(10) as u32);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let wake = Instant::now()
                        .checked_add(delay.min(remaining))
                        .unwrap_or(deadline)
                        .min(deadline);
                    // Keep the actual Windows failure across the final backoff.
                    // Otherwise a missing endpoint and a busy endpoint both
                    // become an unhelpful, source-less deadline error.
                    last_connect_error = Some(error);
                    sleep_until(wake).await;
                    continue;
                }
                Err(error) => {
                    return Err(self.connection_error(error, attempt + 1, false));
                }
            };
            // CreateFile is synchronous. A slow open must not allow a write
            // after the deadline merely because timeout cannot preempt it.
            ensure!(
                Instant::now() < deadline,
                "IPC deadline expired while connecting; request not sent"
            );
            let (response_kind, payload) = timeout_at(deadline, read_response(&mut conn, framed))
                .await
                .context(
                    "IPC response deadline expired; request may have been processed and was not replayed",
                )?
                .context(
                    "IPC exchange failed; request may have been processed and was not replayed",
                )?;
            // Decode stays inside the deadline check. A late success is not returned.
            let response = decode(response_kind, payload).context(
                "IPC exchange failed; request may have been processed and was not replayed",
            )?;
            ensure!(
                Instant::now() < deadline,
                "IPC response arrived after its deadline; request was not replayed"
            );
            if attempt > 0 {
                reconnect_counter().fetch_add(1, Ordering::Relaxed);
            }
            return Ok(response);
        }
        unreachable!("the last connection attempt returns success or its error")
    }

    fn connection_error(
        &self,
        error: std::io::Error,
        attempts: u64,
        deadline_expired: bool,
    ) -> anyhow::Error {
        let reason = match error.raw_os_error() {
            Some(2) => "endpoint not found; the service may be stopped or still starting",
            Some(231) => "all pipe instances are busy; retry when service capacity is available",
            Some(5) => "access denied; check the installed service and named-pipe permissions",
            _ => "the named-pipe connection failed",
        };
        let outcome = if deadline_expired {
            "IPC connection deadline expired"
        } else {
            "cannot connect to IPC pipe"
        };
        anyhow::Error::new(error).context(format!(
            "{outcome} {} after {attempts} attempt(s): {reason}; request not sent",
            self.pipe_name
        ))
    }
}

async fn read_response(
    conn: &mut (impl AsyncRead + AsyncWrite + Unpin),
    framed: &[u8],
) -> Result<(framing::MessageKind, Vec<u8>)> {
    conn.write_all(framed).await?;
    let mut length = [0; 4];
    conn.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    ensure!(
        length != 0 && length <= framing::MAX_FRAME,
        "invalid IPC response length {length}"
    );
    let mut message = vec![0; length];
    conn.read_exact(&mut message).await?;
    let (response_kind, payload) = framing::decode_message(&message)?;
    Ok((response_kind, payload.to_vec()))
}

fn validate_response_id(expected: Uuid, actual: Uuid) -> Result<()> {
    ensure!(
        actual == expected,
        "IPC response {actual} does not match request {expected}"
    );
    Ok(())
}

fn validate_control_response(
    expected: Uuid,
    actual: Uuid,
    success: bool,
    message: Option<&str>,
) -> Result<()> {
    validate_response_id(expected, actual)?;
    ensure!(
        success,
        "service rejected control request: {}",
        message.unwrap_or("no reason supplied")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(windows))]
    use crate::{
        ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse, SearchRequest,
        SearchResponse, StatusRequest, StatusResponse,
    };
    #[cfg(windows)]
    use tokio::net::windows::named_pipe::ServerOptions;

    async fn receive_request(stream: &mut tokio::io::DuplexStream) -> Result<()> {
        let mut length = [0; 4];
        stream.read_exact(&mut length).await?;
        let length = u32::from_le_bytes(length) as usize;
        ensure!(length <= framing::MAX_FRAME, "oversized fixture request");
        let mut message = vec![0; length];
        stream.read_exact(&mut message).await?;
        framing::decode_message(&message)?;
        Ok(())
    }

    fn response_frame(kind: framing::MessageKind, id: Uuid) -> Result<Vec<u8>> {
        let response = ReloadConfigResponse {
            id,
            success: true,
            message: None,
        };
        framing::encode_frame(&framing::encode_message(
            kind,
            &bincode::serialize(&response)?,
        )?)
    }

    #[tokio::test]
    async fn transient_connect_failures_retry_but_send_only_once() -> Result<()> {
        let client = PipeClient::default()
            .with_backoff(Duration::ZERO)
            .with_request_timeout(Duration::from_secs(2));
        let id = Uuid::new_v4();
        let (connection, mut peer) = tokio::io::duplex(4096);
        let mut connection = Some(connection);
        let mut attempts = 0;
        let request = ReloadConfigRequest { id };
        let sending = client.request_with::<_, ReloadConfigResponse, _>(
            framing::MessageKind::ReloadConfig,
            &request,
            || {
                attempts += 1;
                match attempts {
                    1 => Err(std::io::Error::from_raw_os_error(2)),
                    2 => Err(std::io::Error::from_raw_os_error(231)),
                    _ => Ok(connection.take().expect("only one successful open")),
                }
            },
        );
        let serving = async {
            receive_request(&mut peer).await?;
            peer.write_all(&response_frame(framing::MessageKind::ReloadConfig, id)?)
                .await?;
            let mut extra = [0];
            ensure!(
                peer.read(&mut extra).await? == 0,
                "duplicate request after success"
            );
            Ok::<_, anyhow::Error>(())
        };
        let (response, served) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(sending, serving)
        })
        .await?;
        served?;
        assert_eq!(response?.id, id);
        assert_eq!(attempts, 3);
        Ok(())
    }

    #[tokio::test]
    async fn received_request_is_not_replayed_after_lost_or_malformed_reply() -> Result<()> {
        for kind in [
            framing::MessageKind::ReloadConfig,
            framing::MessageKind::Rescan,
            framing::MessageKind::Search,
        ] {
            for malformed in [false, true] {
                let client = PipeClient::default()
                    .with_backoff(Duration::ZERO)
                    .with_request_timeout(Duration::from_secs(2));
                let (connection, mut peer) = tokio::io::duplex(4096);
                let mut connection = Some(connection);
                let mut attempts = 0;
                // These control requests consist of a UUID; the transport is
                // independent of the request model and server query execution.
                let request = ReloadConfigRequest { id: Uuid::new_v4() };
                let sending =
                    client.request_with::<_, ReloadConfigResponse, _>(kind, &request, || {
                        attempts += 1;
                        connection
                            .take()
                            .ok_or_else(|| std::io::Error::from_raw_os_error(2))
                    });
                let serving = async {
                    receive_request(&mut peer).await?;
                    if malformed {
                        peer.write_all(&((framing::MAX_FRAME + 1) as u32).to_le_bytes())
                            .await?;
                    }
                    drop(peer);
                    Ok::<_, anyhow::Error>(())
                };
                let (result, served) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(sending, serving)
                })
                .await?;
                served?;
                assert!(result.unwrap_err().to_string().contains("not replayed"));
                assert_eq!(attempts, 1);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn response_timeout_does_not_duplicate_an_already_received_control() -> Result<()> {
        let client = PipeClient::default()
            .with_backoff(Duration::ZERO)
            .with_request_timeout(Duration::from_millis(100));
        let (connection, mut peer) = tokio::io::duplex(4096);
        let mut connection = Some(connection);
        let mut attempts = 0;
        let request = RescanRequest { id: Uuid::new_v4() };
        let sending = client.request_with::<_, RescanResponse, _>(
            framing::MessageKind::Rescan,
            &request,
            || {
                attempts += 1;
                connection
                    .take()
                    .ok_or_else(|| std::io::Error::from_raw_os_error(2))
            },
        );
        let serving = async {
            receive_request(&mut peer).await?;
            // Do not reply. EOF proves the timed-out invocation closed its
            // connection instead of sending another control on it.
            let mut extra = [0];
            ensure!(
                peer.read(&mut extra).await? == 0,
                "duplicate control after timeout"
            );
            Ok::<_, anyhow::Error>(())
        };
        let (result, served) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(sending, serving)
        })
        .await?;
        served?;
        assert!(result.unwrap_err().to_string().contains("not replayed"));
        assert_eq!(attempts, 1);
        Ok(())
    }

    #[tokio::test]
    async fn connection_failures_and_extreme_budgets_do_not_panic_or_resend() -> Result<()> {
        let request = StatusRequest { id: Uuid::new_v4() };
        let mut attempts = 0;
        let denied = PipeClient::default().with_retries(u32::MAX);
        let error = denied
            .request_with::<_, StatusResponse, tokio::io::DuplexStream>(
                framing::MessageKind::Status,
                &request,
                || {
                    attempts += 1;
                    Err(std::io::Error::from_raw_os_error(5))
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("request not sent"));
        assert_eq!(attempts, 1);
        for budget in [Duration::ZERO, Duration::MAX] {
            let client = PipeClient::default().with_request_timeout(budget);
            let result = client
                .request_with::<_, StatusResponse, tokio::io::DuplexStream>(
                    framing::MessageKind::Status,
                    &request,
                    || panic!("invalid deadline must not open"),
                )
                .await;
            assert!(result.is_err());
        }
        let client = PipeClient::default()
            .with_request_timeout(Duration::from_millis(10))
            .with_backoff(Duration::MAX)
            .with_retries(u32::MAX);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.request_with::<_, StatusResponse, tokio::io::DuplexStream>(
                framing::MessageKind::Status,
                &request,
                || Err(std::io::Error::from_raw_os_error(231)),
            ),
        )
        .await?;
        assert!(result.unwrap_err().to_string().contains("request not sent"));
        Ok(())
    }

    #[tokio::test]
    async fn exhausted_backoff_preserves_missing_and_busy_pipe_errors() {
        for (code, expected) in [(2, "endpoint not found"), (231, "pipe instances are busy")] {
            let client = PipeClient::new(r"\\.\pipe\ultrasearch-startup-test")
                .with_request_timeout(Duration::from_millis(20))
                .with_backoff(Duration::from_secs(1));
            let mut attempts = 0;
            let error = client
                .request_with::<_, StatusResponse, tokio::io::DuplexStream>(
                    framing::MessageKind::Status,
                    &StatusRequest { id: Uuid::new_v4() },
                    || {
                        attempts += 1;
                        Err(std::io::Error::from_raw_os_error(code))
                    },
                )
                .await
                .unwrap_err();
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("deadline expired"), "{diagnostic}");
            assert!(diagnostic.contains("ultrasearch-startup-test"));
            assert!(diagnostic.contains(expected), "{diagnostic}");
            assert!(diagnostic.contains("request not sent"));
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(code)
            );
            assert_eq!(attempts, 1, "deadline must bound all retry backoff");
        }
    }

    #[tokio::test]
    async fn cold_search_can_finish_after_the_short_probe_budget_without_replay() -> Result<()> {
        let client = PipeClient::default().with_request_timeout(Duration::from_secs(2));
        let request = SearchRequest {
            id: Uuid::new_v4(),
            ..SearchRequest::default()
        };
        let id = request.id;
        let (connection, mut peer) = tokio::io::duplex(4096);
        let mut connection = Some(connection);
        let mut attempts = 0;
        let sending = client.request_with::<_, SearchResponse, _>(
            framing::MessageKind::Search,
            &request,
            || {
                attempts += 1;
                connection
                    .take()
                    .ok_or_else(|| std::io::Error::from_raw_os_error(2))
            },
        );
        let serving = async {
            receive_request(&mut peer).await?;
            tokio::time::sleep(Duration::from_millis(DEFAULT_TIMEOUT_MS + 100)).await;
            let response = SearchResponse {
                id,
                hits: Vec::new(),
                total: 0,
                truncated: false,
                took_ms: 850,
                served_by: Some("cold-index".into()),
            };
            peer.write_all(&framing::encode_frame(&framing::encode_message(
                framing::MessageKind::Search,
                &bincode::serialize(&response)?,
            )?)?)
            .await?;
            Ok::<_, anyhow::Error>(())
        };
        let (response, served) = tokio::join!(sending, serving);
        served?;
        response?.validate_for(id)?;
        assert_eq!(attempts, 1);
        Ok(())
    }

    #[test]
    fn control_validation_requires_matching_identity_and_affirmative_acknowledgement() {
        let id = Uuid::new_v4();
        validate_control_response(id, id, true, None).unwrap();
        assert!(validate_control_response(id, Uuid::new_v4(), true, None).is_err());
        let error = validate_control_response(id, id, false, Some("restart required")).unwrap_err();
        assert!(error.to_string().contains("restart required"));
        assert!(validate_control_response(id, id, false, None).is_err());
        assert!(validate_response_id(id, Uuid::new_v4()).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_control_calls_reject_negative_and_unrelated_acknowledgements() -> Result<()> {
        for kind in [
            framing::MessageKind::ReloadConfig,
            framing::MessageKind::Rescan,
        ] {
            for (success, matching) in [(true, true), (false, true), (true, false)] {
                let id = Uuid::new_v4();
                let response_id = if matching { id } else { Uuid::new_v4() };
                let name = format!(r"\\.\pipe\ultrasearch-control-{}", Uuid::new_v4());
                let mut server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&name)?;
                let client = PipeClient::new(name).with_request_timeout(Duration::from_secs(2));
                let serving = async {
                    server.connect().await?;
                    let mut length = [0; 4];
                    server.read_exact(&mut length).await?;
                    let length = u32::from_le_bytes(length) as usize;
                    ensure!(length <= framing::MAX_FRAME, "oversized test request");
                    let mut message = vec![0; length];
                    server.read_exact(&mut message).await?;
                    let (actual_kind, payload) = framing::decode_message(&message)?;
                    ensure!(actual_kind == kind, "wrong control request kind");
                    let message = Some("restart required; active configuration unchanged".into());
                    let payload = match kind {
                        framing::MessageKind::ReloadConfig => {
                            let request: ReloadConfigRequest = bincode::deserialize(payload)?;
                            ensure!(request.id == id, "wrong reload identity");
                            bincode::serialize(&ReloadConfigResponse {
                                id: response_id,
                                success,
                                message,
                            })?
                        }
                        framing::MessageKind::Rescan => {
                            let request: RescanRequest = bincode::deserialize(payload)?;
                            ensure!(request.id == id, "wrong rescan identity");
                            bincode::serialize(&RescanResponse {
                                id: response_id,
                                success,
                                message,
                            })?
                        }
                        _ => unreachable!(),
                    };
                    server
                        .write_all(&framing::encode_frame(&framing::encode_message(
                            kind, &payload,
                        )?)?)
                        .await?;
                    Ok::<_, anyhow::Error>(())
                };
                let requesting = async {
                    match kind {
                        framing::MessageKind::ReloadConfig => client
                            .reload_config(ReloadConfigRequest { id })
                            .await
                            .map(|_| ()),
                        framing::MessageKind::Rescan => {
                            client.rescan(RescanRequest { id }).await.map(|_| ())
                        }
                        _ => unreachable!(),
                    }
                };
                let (served, result) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(serving, requesting)
                })
                .await?;
                served?;
                if success && matching {
                    result?;
                } else {
                    let message = result.unwrap_err().to_string();
                    assert!(message.contains(if matching {
                        "restart required"
                    } else {
                        "does not match request"
                    }));
                }
            }
        }
        Ok(())
    }

    /// Exercise the real client, versioned framing, and a local native pipe.
    /// The outer deadline also bounds a server waiting after client failure.
    #[cfg(windows)]
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

    #[cfg(windows)]
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

    #[cfg(windows)]
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
