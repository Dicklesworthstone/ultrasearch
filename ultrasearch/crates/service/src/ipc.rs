#![cfg(any(target_os = "windows", test))]

#[cfg(windows)]
use std::env;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[cfg(windows)]
use crate::metrics::{global_metrics_snapshot, record_ipc_request};
#[cfg(windows)]
use crate::search_handler::search;
#[cfg(windows)]
use crate::status::make_status_response;
#[cfg(windows)]
use crate::status_provider::status_snapshot;
use anyhow::{Context, Result};
#[cfg(windows)]
use ipc::{
    MetricsSnapshot, ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse,
    SearchRequest, StatusRequest,
};
#[cfg(all(windows, test))]
use ipc::StatusResponse;
use ipc::{SearchResponse, framing};
#[cfg(windows)]
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::sync::Semaphore;
#[cfg(windows)]
use tokio::task::JoinHandle;
use uuid::Uuid;

#[cfg(windows)]
const DEFAULT_PIPE_NAME: &str = r#"\\.\pipe\ultrasearch"#;
const MAX_MESSAGE_BYTES: usize = framing::MAX_FRAME;
#[cfg(windows)]
const MAX_CONNECTIONS: usize = 32;
#[cfg(windows)]
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DISPATCH_TIME: Duration = Duration::from_secs(30);

// A cancelled connection or listener must not reset the computation budget
// while detached blocking queries still run. Control requests have a separate
// lane, so a saturated search executor cannot consume their dispatch permits.
struct DispatchLanes {
    search: Arc<Semaphore>,
    control: Arc<Semaphore>,
}

impl Default for DispatchLanes {
    fn default() -> Self {
        Self {
            search: Arc::new(Semaphore::new(4)),
            control: Arc::new(Semaphore::new(2)),
        }
    }
}

impl DispatchLanes {
    fn for_kind(&self, kind: framing::MessageKind) -> Arc<Semaphore> {
        Arc::clone(if kind == framing::MessageKind::Search {
            &self.search
        } else {
            &self.control
        })
    }
}

#[cfg(windows)]
static DISPATCH_LANES: OnceLock<DispatchLanes> = OnceLock::new();
#[cfg(windows)]
static CONNECTION_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Bind the first pipe instance before reporting successful startup. Keep a
/// replacement listener alive before handing a connected instance to a task.
#[cfg(windows)]
pub async fn start_pipe_server(pipe_name: Option<&str>) -> Result<JoinHandle<()>> {
    let slots =
        Arc::clone(CONNECTION_SLOTS.get_or_init(|| Arc::new(Semaphore::new(MAX_CONNECTIONS))));
    start_pipe_server_with_slots(pipe_name, slots).await
}

#[cfg(windows)]
async fn start_pipe_server_with_slots(
    pipe_name: Option<&str>,
    slots: Arc<Semaphore>,
) -> Result<JoinHandle<()>> {
    let name = pipe_name.unwrap_or(DEFAULT_PIPE_NAME).to_string();
    anyhow::ensure!(
        !name.contains('\0'),
        "named pipe name must not contain an embedded NUL"
    );
    // Binding inside the spawned task used to return Ok even when another
    // service owned this name or the security descriptor could not be applied.
    // The caller must see that failure before starting ingestion workers.
    let mut server = unsafe { create_secure_pipe(&name, true) }
        .with_context(|| format!("cannot bind UltraSearch named pipe {name}"))?;

    let handle = tokio::spawn(async move {
        loop {
            // One listener stays alive while saturated, but no extra handler,
            // request allocation or blocking query is admitted without a slot.
            let permit = match Arc::clone(&slots).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => return,
            };
            let connected = server.connect().await;
            // Retain the current instance until its successor exists. Otherwise
            // a short-lived client can close the last handle before the next
            // listener is created, losing both availability and name ownership.
            // https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.NamedPipeServer.html
            let next = loop {
                match unsafe { create_secure_pipe(&name, false) } {
                    Ok(next) => break next,
                    Err(error) => {
                        tracing::warn!(%error, "named pipe replacement failed; retaining current instance and retrying in 1s");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            };
            let accepted = std::mem::replace(&mut server, next);
            if let Err(e) = connected {
                tracing::error!("named pipe connect failed: {}", e);
                drop(accepted);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }

            tokio::spawn(async move {
                let _connection_permit = permit;
                if let Err(e) = handle_connection(accepted).await {
                    tracing::warn!("pipe connection error: {e:?}");
                }
            });
        }
    });

    Ok(handle)
}

#[cfg(windows)]
unsafe fn create_secure_pipe(name: &str, first: bool) -> Result<NamedPipeServer> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
    use windows::Win32::Security::{
        Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW, PSECURITY_DESCRIPTOR,
        SECURITY_ATTRIBUTES,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
        PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        CreateNamedPipeW, NAMED_PIPE_MODE, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows::core::PCWSTR;

    // D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)
    let sddl = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)\0";
    let sddl_wide: Vec<u16> = sddl.encode_utf16().collect();

    let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();

    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_wide.as_ptr()),
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        )?;
    }

    // Ensure we free the SD
    struct SdGuard(PSECURITY_DESCRIPTOR);
    impl Drop for SdGuard {
        fn drop(&mut self) {
            // sd.0 is *mut c_void. HLOCAL wraps *mut c_void.
            unsafe {
                let _ = LocalFree(HLOCAL(self.0.0));
            }
        }
    }
    let _guard = SdGuard(sd);

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 as *mut _,
        bInheritHandle: windows::Win32::Foundation::FALSE,
    };

    let mut name_wide: Vec<u16> = OsStr::new(name).encode_wide().collect();
    name_wide.push(0);

    let mut open_mode = PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_OVERLAPPED.0;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE.0;
    }

    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name_wide.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(open_mode),
            NAMED_PIPE_MODE(
                PIPE_TYPE_BYTE.0
                    | PIPE_READMODE_BYTE.0
                    | PIPE_WAIT.0
                    | PIPE_REJECT_REMOTE_CLIENTS.0,
            ),
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            Some(&sa),
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        return Err(anyhow::Error::from(windows::core::Error::from_win32()));
    }

    // Wrap in Tokio
    let server = unsafe { NamedPipeServer::from_raw_handle(handle.0 as *mut _) }?;
    Ok(server)
}

#[cfg(windows)]
async fn handle_connection(mut conn: NamedPipeServer) -> Result<()> {
    while let Some(message) = read_request_frame(&mut conn, IO_TIMEOUT).await? {
        // Reject old identity layouts before deserializing a request or
        // publishing search hits that an old UI could misinterpret.
        let (kind, payload) = framing::decode_message(&message)?;
        let search_request = if kind == framing::MessageKind::Search {
            let request =
                deserialize_exact::<SearchRequest>(payload).context("invalid search request")?;
            Some((request.id, request.timeout))
        } else {
            None
        };
        let budget = search_request
            .and_then(|(_, timeout)| timeout)
            .unwrap_or(MAX_DISPATCH_TIME)
            .min(MAX_DISPATCH_TIME);
        let started = Instant::now();
        let lane = DISPATCH_LANES
            .get_or_init(DispatchLanes::default)
            .for_kind(kind);
        let result = run_bounded_dispatch(lane, budget, move || {
            let (_, payload) = framing::decode_message(&message)?;
            let response = dispatch(kind, payload);
            anyhow::ensure!(!response.is_empty(), "invalid {kind:?} IPC payload");
            Ok(response)
        })
        .await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, ?kind, "IPC request could not complete");
                let Some((id, _)) = search_request else {
                    // Never manufacture a successful control acknowledgement.
                    return Err(error);
                };
                unavailable_search(id, started.elapsed())?
            }
        };
        write_response_frame(&mut conn, kind, &response, IO_TIMEOUT).await?;
    }
    Ok(())
}

/// Use one deadline for the entire header and body, not one per short read.
/// Only EOF before any header byte is a clean disconnect. A partial frame is
/// an error and must never be reused as the prefix of the next request.
async fn read_request_frame<R: AsyncRead + Unpin>(
    conn: &mut R,
    budget: Duration,
) -> Result<Option<Vec<u8>>> {
    anyhow::ensure!(!budget.is_zero(), "IPC request frame deadline exceeded");
    tokio::time::timeout(budget, async {
        let mut prefix = [0; 4];
        if conn.read(&mut prefix[..1]).await? == 0 {
            return Ok(None);
        }
        conn.read_exact(&mut prefix[1..])
            .await
            .context("incomplete IPC frame header")?;
        let length = u32::from_le_bytes(prefix) as usize;
        anyhow::ensure!(
            length > 0 && length <= MAX_MESSAGE_BYTES,
            "invalid IPC frame size {length}"
        );
        let mut message = vec![0; length];
        conn.read_exact(&mut message)
            .await
            .context("incomplete IPC frame body")?;
        Ok(Some(message))
    })
    .await
    .context("IPC request frame deadline exceeded")?
}

async fn write_response_frame<W: AsyncWrite + Unpin>(
    conn: &mut W,
    kind: framing::MessageKind,
    payload: &[u8],
    budget: Duration,
) -> Result<()> {
    anyhow::ensure!(!budget.is_zero(), "IPC response write deadline exceeded");
    let frame = framing::encode_frame(&framing::encode_message(kind, payload)?)?;
    tokio::time::timeout(budget, conn.write_all(&frame))
        .await
        .context("IPC response write deadline exceeded")?
        .context("cannot write IPC response")
}

/// Bound admission AND the response wait. A started blocking query cannot be
/// killed by cancelling its JoinHandle. Its owned permit therefore lives in
/// the closure, not in this async waiter, until the query actually finishes.
/// https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
async fn run_bounded_dispatch<T, F>(lane: Arc<Semaphore>, budget: Duration, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    anyhow::ensure!(!budget.is_zero(), "IPC request processing deadline exceeded");
    let started = Instant::now();
    tokio::time::timeout(budget, async move {
        let permit = lane
            .acquire_owned()
            .await
            .context("IPC dispatch lane closed")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // The blocking pool may itself have been saturated. Do not start
            // a queued computation whose response deadline has already passed.
            anyhow::ensure!(
                started.elapsed() < budget,
                "IPC dispatch expired before execution"
            );
            let result = work();
            anyhow::ensure!(
                started.elapsed() < budget,
                "IPC dispatch completed after its deadline"
            );
            result
        })
        .await
        .context("IPC blocking dispatch failed")?
    })
    .await
    .context("IPC request processing deadline exceeded")?
}

fn unavailable_search(id: Uuid, elapsed: Duration) -> Result<Vec<u8>> {
    framing::serialize_search_response(&mut SearchResponse {
        id,
        hits: Vec::new(),
        total: 0,
        truncated: true,
        took_ms: elapsed.as_millis().min(u32::MAX as u128) as u32,
        served_by: Some("service-unavailable".into()),
    })
}

#[cfg(windows)]
fn deserialize_exact<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
    let mut cursor = Cursor::new(payload);
    match bincode::deserialize_from::<_, T>(&mut cursor) {
        Ok(v) if cursor.position() as usize == payload.len() => Some(v),
        _ => None,
    }
}

#[cfg(windows)]
fn dispatch(kind: framing::MessageKind, payload: &[u8]) -> Vec<u8> {
    // Fast-path: ping echo when payload is prefixed with "PING" + UUID.
    if kind == framing::MessageKind::Ping
        && payload.len() == 20
        && payload.starts_with(b"PING")
        && let Ok(id) = Uuid::from_slice(&payload[4..20])
    {
        return id.as_bytes().to_vec();
    }

    if kind == framing::MessageKind::Status
        && let Some(req) = deserialize_exact::<StatusRequest>(payload)
    {
        let started = Instant::now();
        let snap = status_snapshot();
        let empty_metrics =
            snap.metrics.or(
                global_metrics_snapshot(Some(0), Some(0), Some(0), Some(0)).or(Some(
                    MetricsSnapshot {
                        search_latency_ms_p50: None,
                        search_latency_ms_p95: None,
                        worker_cpu_pct: None,
                        worker_mem_bytes: None,
                        queue_depth: Some(0),
                        active_workers: Some(0),
                        content_enqueued: Some(0),
                        content_dropped: Some(0),
                    },
                )),
            );
        let resp = make_status_response(
            req.id,
            snap.volumes,
            snap.scheduler_state,
            empty_metrics,
            snap.last_index_commit_ts,
            snap.content_jobs_total,
            snap.content_jobs_remaining,
            snap.content_bytes_total,
            snap.content_bytes_remaining,
        );
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(started.elapsed());
        return encoded;
    }

    // Handle ReloadConfigRequest
    if kind == framing::MessageKind::ReloadConfig
        && let Some(req) = deserialize_exact::<ReloadConfigRequest>(payload)
    {
        let started = Instant::now();
        let result = core_types::config::reload_config(None);
        let (success, message) = match result {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let resp = ReloadConfigResponse {
            id: req.id,
            success,
            message,
        };
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(started.elapsed());
        return encoded;
    }

    // Handle RescanRequest
    if kind == framing::MessageKind::Rescan
        && let Some(req) = deserialize_exact::<RescanRequest>(payload)
    {
        let started = Instant::now();
        crate::scanner::request_rescan();
        let success = true;
        let message = Some("Reconciliation requested on the durable indexing lane; check ingestion status for completion".to_string());

        let resp = RescanResponse {
            id: req.id,
            success,
            message,
        };
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(started.elapsed());
        return encoded;
    }

    if kind == framing::MessageKind::Search
        && let Some(req) = deserialize_exact::<SearchRequest>(payload)
    {
        let start = Instant::now();
        let req_clone = req.clone();
        let mut resp = search(req);
        // Ensure the echoed id always matches the request for protocol stability.
        // search(req) should propagate id, but we enforce it defensively.
        // Use the id already in resp if set, otherwise fallback to request id.
        if resp.id.is_nil() {
            resp.id = req_clone.id;
        }
        let elapsed = start.elapsed();
        let took = elapsed.as_millis().min(u32::MAX as u128) as u32;
        if resp.took_ms == 0 {
            resp.took_ms = took;
        }
        if resp.served_by.is_none() {
            resp.served_by = Some(host_label());
        }
        let encoded = match framing::serialize_search_response(&mut resp) {
            Ok(encoded) => encoded,
            Err(error) => {
                tracing::warn!(%error, "search page cannot be encoded; reporting unavailable");
                resp.hits.clear();
                resp.truncated = true;
                resp.served_by = Some("service-unavailable".into());
                // The fallback contains only fixed fields and a bounded label;
                // it cannot retain an invalid identity or oversized stored row.
                framing::serialize_search_response(&mut resp).unwrap_or_default()
            }
        };
        record_ipc_request(elapsed);
        return encoded;
    }
    Vec::new()
}

#[cfg(windows)]
fn host_label() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "service".into())
}

#[cfg(all(windows, test))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_pipe_connection_backpressure_releases_after_disconnect() {
        use tokio::net::windows::named_pipe::ClientOptions;

        let name = format!(r"\\.\pipe\ultrasearch-capacity-{}", Uuid::new_v4());
        let listener = start_pipe_server_with_slots(Some(&name), Arc::new(Semaphore::new(1)))
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let id = Uuid::new_v4();
            let mut payload = b"PING".to_vec();
            payload.extend_from_slice(id.as_bytes());
            let kind = framing::MessageKind::Ping;
            let mut first = ClientOptions::new().open(&name).unwrap();
            write_response_frame(&mut first, kind, &payload, IO_TIMEOUT)
                .await
                .unwrap();
            let first_reply = read_request_frame(&mut first, IO_TIMEOUT)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                framing::decode_message(&first_reply).unwrap().1,
                id.as_bytes()
            );

            // The first connection retains its only slot while waiting for its
            // next frame. A second kernel connection must not create a handler.
            let mut second = ClientOptions::new().open(&name).unwrap();
            write_response_frame(&mut second, kind, &payload, IO_TIMEOUT)
                .await
                .unwrap();
            let blocked = read_request_frame(&mut second, Duration::from_millis(25)).await;
            assert!(blocked.unwrap_err().to_string().contains("deadline"));
            drop(first);
            let response = read_request_frame(&mut second, IO_TIMEOUT)
                .await
                .unwrap()
                .unwrap();
            let (response_kind, response_payload) = framing::decode_message(&response).unwrap();
            assert_eq!(response_kind, kind);
            assert_eq!(response_payload, id.as_bytes());
        })
        .await;
        listener.abort();
        let _ = listener.await;
        result.expect("native connection capacity did not recover after disconnect");
    }

    #[tokio::test]
    async fn native_pipe_zero_budget_search_is_explicitly_unavailable() {
        let name = format!(r"\\.\pipe\ultrasearch-deadline-{}", Uuid::new_v4());
        let listener = start_pipe_server(Some(&name)).await.unwrap();
        let client = ipc::client::PipeClient::new(name)
            .with_retries(0)
            .with_request_timeout(Duration::from_secs(2));
        let result = client
            .search(SearchRequest {
                id: Uuid::new_v4(),
                timeout: Some(Duration::ZERO),
                ..SearchRequest::default()
            })
            .await;
        listener.abort();
        let _ = listener.await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("could not complete this search")
        );
    }

    #[tokio::test]
    async fn native_pipe_startup_rejects_duplicate_and_invalid_names() {
        let name = format!(r"\\.\pipe\ultrasearch-bind-{}", Uuid::new_v4());
        let listener = start_pipe_server(Some(&name)).await.unwrap();
        let duplicate = start_pipe_server(Some(&name)).await;
        assert!(
            duplicate.is_err(),
            "startup must not acknowledge a failed bind"
        );
        assert!(start_pipe_server(Some("not-a-pipe-name")).await.is_err());
        assert!(
            start_pipe_server(Some(&format!("{name}\0suffix")))
                .await
                .is_err()
        );
        listener.abort();
        assert!(listener.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn native_pipe_keeps_name_ownership_across_short_lived_clients() {
        use tokio::net::windows::named_pipe::ClientOptions;

        let name = format!(r"\\.\pipe\ultrasearch-churn-{}", Uuid::new_v4());
        let listener = start_pipe_server(Some(&name)).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for _ in 0..32 {
                let mut client = ClientOptions::new().open(&name).unwrap();
                let id = Uuid::new_v4();
                let mut payload = b"PING".to_vec();
                payload.extend_from_slice(id.as_bytes());
                let message =
                    framing::encode_message(framing::MessageKind::Ping, &payload).unwrap();
                client
                    .write_all(&framing::encode_frame(&message).unwrap())
                    .await
                    .unwrap();
                let mut prefix = [0; 4];
                client.read_exact(&mut prefix).await.unwrap();
                let length = u32::from_le_bytes(prefix) as usize;
                assert!(length > 0 && length <= MAX_MESSAGE_BYTES);
                let mut response = vec![0; length];
                client.read_exact(&mut response).await.unwrap();
                let (kind, payload) = framing::decode_message(&response).unwrap();
                assert_eq!(kind, framing::MessageKind::Ping);
                assert_eq!(payload, id.as_bytes());
                drop(client);
                tokio::task::yield_now().await;
                assert!(
                    unsafe { create_secure_pipe(&name, true) }.is_err(),
                    "a replacement listener must retain exclusive name ownership"
                );
            }
        })
        .await;
        listener.abort();
        let _ = listener.await;
        result.expect("native named-pipe churn exceeded its deadline");
    }

    #[tokio::test]
    async fn echoes_uuid_prefix() {
        let id = Uuid::new_v4();
        let mut payload = b"PING".to_vec();
        payload.extend_from_slice(id.as_bytes());
        let resp = dispatch(framing::MessageKind::Ping, &payload);
        assert_eq!(resp, id.as_bytes());
    }

    #[test]
    fn status_request_roundtrip() {
        let req = StatusRequest { id: Uuid::new_v4() };
        let resp_bytes = dispatch(
            framing::MessageKind::Status,
            &bincode::serialize(&req).unwrap(),
        );
        let resp: StatusResponse = bincode::deserialize(&resp_bytes).unwrap();
        assert_eq!(resp.id, req.id);
        assert!(resp.volumes.is_empty());
        assert_eq!(resp.metrics.as_ref().and_then(|m| m.queue_depth), Some(0));
        assert!(resp.served_by.is_some());
    }

    #[test]
    fn search_request_echoes_id() {
        let req = SearchRequest {
            id: Uuid::new_v4(),
            query: ipc::QueryExpr::Term(ipc::TermExpr {
                field: None,
                value: "x".into(),
                modifier: ipc::TermModifier::Term,
            }),
            limit: 1,
            mode: ipc::SearchMode::Auto,
            timeout: None,
            offset: 0,
        };
        let resp_bytes = dispatch(
            framing::MessageKind::Search,
            &bincode::serialize(&req).unwrap(),
        );
        let resp: SearchResponse = bincode::deserialize(&resp_bytes).unwrap();
        assert_eq!(resp.id, req.id);
        assert!(resp.hits.is_empty());
        assert_eq!(resp.total, 0);
    }

    #[test]
    fn rescan_request_is_not_misrouted_to_status() {
        let req = RescanRequest { id: Uuid::new_v4() };
        let payload = bincode::serialize(&req).unwrap();
        assert_eq!(
            payload,
            bincode::serialize(&StatusRequest { id: req.id }).unwrap()
        );
        let response = dispatch(framing::MessageKind::Rescan, &payload);
        let response: RescanResponse = bincode::deserialize(&response).unwrap();
        assert_eq!(response.id, req.id);
        assert!(response.success);
        assert!(
            response
                .message
                .as_deref()
                .unwrap()
                .contains("Reconciliation requested")
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn bounded_transport_roundtrips_short_reads_and_clean_eof() {
        let (mut writer, mut reader) = tokio::io::duplex(3);
        let payload = b"PING0123456789abcdef";
        let (sent, received) = tokio::join!(
            write_response_frame(
                &mut writer,
                framing::MessageKind::Ping,
                payload,
                MAX_DISPATCH_TIME,
            ),
            read_request_frame(&mut reader, MAX_DISPATCH_TIME),
        );
        sent.unwrap();
        let message = received.unwrap().unwrap();
        let (kind, decoded) = framing::decode_message(&message).unwrap();
        assert_eq!(kind, framing::MessageKind::Ping);
        assert_eq!(decoded, payload);
        writer.shutdown().await.unwrap();
        assert!(
            read_request_frame(&mut reader, MAX_DISPATCH_TIME)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn invalid_lengths_and_partial_eof_are_not_clean_disconnects() {
        for length in [0, (MAX_MESSAGE_BYTES + 1) as u32, u32::MAX] {
            let prefix = length.to_le_bytes();
            let mut reader = prefix.as_slice();
            let error = read_request_frame(&mut reader, MAX_DISPATCH_TIME)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("invalid IPC frame size"));
        }
        for bytes in [vec![1], vec![1, 0, 0], vec![3, 0, 0, 0, 1, 2]] {
            let mut reader = bytes.as_slice();
            let error = read_request_frame(&mut reader, MAX_DISPATCH_TIME)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("incomplete IPC frame"));
        }
        let bytes = [1, 0, 0, 0, 7];
        let mut reader = bytes.as_slice();
        assert!(read_request_frame(&mut reader, Duration::ZERO).await.is_err());
        assert_eq!(reader, bytes.as_slice(), "zero budget must not consume a partial frame");
    }

    #[tokio::test]
    async fn stalled_headers_bodies_and_response_consumers_hit_the_deadline() {
        for prefix in [vec![1], vec![4, 0, 0, 0, 1]] {
            let (mut peer, mut reader) = tokio::io::duplex(8);
            peer.write_all(&prefix).await.unwrap();
            // Keep the peer open: these reads can finish only by timing out,
            // not by turning a partial request into a clean disconnection.
            let error = read_request_frame(&mut reader, Duration::from_millis(10))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("deadline"));
            drop(peer);
        }
        let (mut writer, peer) = tokio::io::duplex(1);
        let error = write_response_frame(
            &mut writer,
            framing::MessageKind::Ping,
            b"reply",
            Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("deadline"));
        drop(peer);
    }

    #[tokio::test]
    async fn expired_admission_never_starts_work_or_leaks_a_waiter() {
        for (capacity, budget) in [(0, Duration::from_millis(10)), (1, Duration::ZERO)] {
            let lane = Arc::new(Semaphore::new(capacity));
            let called = Arc::new(AtomicBool::new(false));
            let marker = Arc::clone(&called);
            let result = run_bounded_dispatch(Arc::clone(&lane), budget, move || {
                marker.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
            assert!(result.is_err());
            assert!(!called.load(Ordering::SeqCst));
            lane.add_permits(1);
            assert_eq!(lane.available_permits(), capacity + 1);
        }
    }

    #[tokio::test]
    async fn cancelled_dispatch_retains_capacity_until_the_blocking_owner_finishes() {
        let lane = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(run_bounded_dispatch(
            Arc::clone(&lane),
            MAX_DISPATCH_TIME,
            move || {
                let _ = started_tx.send(());
                release_rx.recv_timeout(Duration::from_secs(5))?;
                Ok(())
            },
        ));
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        // This also proves blocking work did not occupy the current-thread
        // async reactor: it must run here before release_rx is signalled.
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(Arc::clone(&lane).try_acquire_owned().is_err());
        let called = Arc::new(AtomicBool::new(false));
        let marker = Arc::clone(&called);
        let second = tokio::spawn(run_bounded_dispatch(
            Arc::clone(&lane),
            MAX_DISPATCH_TIME,
            move || {
                marker.store(true, Ordering::SeqCst);
                Ok(42)
            },
        ));
        tokio::task::yield_now().await;
        assert!(!called.load(Ordering::SeqCst));
        release_tx.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            42
        );
        assert_eq!(lane.available_permits(), 1);
    }

    #[test]
    fn expired_work_queued_in_the_blocking_pool_is_not_executed_late() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started_tx.send(());
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            tokio::time::timeout(Duration::from_secs(2), started_rx)
                .await
                .unwrap()
                .unwrap();
            let lane = Arc::new(Semaphore::new(1));
            let called = Arc::new(AtomicBool::new(false));
            let marker = Arc::clone(&called);
            let result = run_bounded_dispatch(
                Arc::clone(&lane),
                Duration::from_millis(10),
                move || {
                    marker.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
            assert!(!called.load(Ordering::SeqCst));
            assert!(Arc::clone(&lane).try_acquire_owned().is_err());
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            let permit = tokio::time::timeout(
                Duration::from_secs(2),
                Arc::clone(&lane).acquire_owned(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                !called.load(Ordering::SeqCst),
                "an expired queued query ran after its caller left"
            );
            drop(permit);
        });
    }

    #[tokio::test]
    async fn saturated_search_lane_does_not_consume_control_capacity() {
        let lanes = DispatchLanes::default();
        let search_lane = lanes.for_kind(framing::MessageKind::Search);
        let held = Arc::clone(&search_lane)
            .acquire_many_owned(4)
            .await
            .unwrap();
        for kind in [
            framing::MessageKind::Ping,
            framing::MessageKind::Status,
            framing::MessageKind::Rescan,
            framing::MessageKind::ReloadConfig,
        ] {
            let result = run_bounded_dispatch(lanes.for_kind(kind), MAX_DISPATCH_TIME, || Ok(7))
                .await
                .unwrap();
            assert_eq!(result, 7);
            assert_eq!(search_lane.available_permits(), 0);
        }
        drop(held);
        let failed = run_bounded_dispatch::<(), _>(
            Arc::clone(&search_lane),
            MAX_DISPATCH_TIME,
            || anyhow::bail!("simulated query failure"),
        )
        .await;
        assert!(
            failed
                .unwrap_err()
                .to_string()
                .contains("simulated query failure")
        );
        assert_eq!(search_lane.available_permits(), 4);
    }

    #[tokio::test]
    async fn timed_out_search_response_is_a_bounded_explicit_failure_on_the_wire() {
        let id = Uuid::new_v4();
        let payload = unavailable_search(id, Duration::from_millis(23)).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(8);
        let (sent, received) = tokio::join!(
            write_response_frame(
                &mut writer,
                framing::MessageKind::Search,
                &payload,
                MAX_DISPATCH_TIME,
            ),
            read_request_frame(&mut reader, MAX_DISPATCH_TIME),
        );
        sent.unwrap();
        let message = received.unwrap().unwrap();
        assert!(message.len() <= MAX_MESSAGE_BYTES);
        let (kind, payload) = framing::decode_message(&message).unwrap();
        assert_eq!(kind, framing::MessageKind::Search);
        let response: SearchResponse = bincode::deserialize(payload).unwrap();
        assert_eq!(response.id, id);
        assert_eq!(response.took_ms, 23);
        assert!(response.hits.is_empty());
        assert!(response.truncated);
        assert!(response.validate_for(id).is_err());
    }
}
