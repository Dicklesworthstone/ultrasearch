#![cfg(target_os = "windows")]

use std::env;
use std::time::Instant;

use crate::metrics::{global_metrics_snapshot, record_ipc_request};
use crate::search_handler::search;
use crate::status::make_status_response;
use crate::status_provider::status_snapshot;
use anyhow::{Context, Result};
use ipc::{
    MetricsSnapshot, ReloadConfigRequest, ReloadConfigResponse, RescanRequest, RescanResponse,
    SearchRequest, StatusRequest, framing,
};
#[cfg(test)]
use ipc::{SearchResponse, StatusResponse};
use std::io::Cursor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::task::JoinHandle;
use uuid::Uuid;

const DEFAULT_PIPE_NAME: &str = r#"\\.\pipe\ultrasearch"#;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;

/// Bind the first pipe instance before reporting successful startup. Keep a
/// replacement listener alive before handing a connected instance to a task.
pub async fn start_pipe_server(pipe_name: Option<&str>) -> Result<JoinHandle<()>> {
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
                if let Err(e) = handle_connection(accepted).await {
                    tracing::warn!("pipe connection error: {e:?}");
                }
            });
        }
    });

    Ok(handle)
}

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

async fn handle_connection(mut conn: NamedPipeServer) -> Result<()> {
    loop {
        // decode frame
        let mut len_prefix = [0u8; 4];
        // If read returns 0, client disconnected (or EOF).
        if conn.read_exact(&mut len_prefix).await.is_err() {
            break;
        }
        let frame_len = u32::from_le_bytes(len_prefix) as usize;
        if frame_len == 0 || frame_len > MAX_MESSAGE_BYTES {
            tracing::warn!("invalid frame size {frame_len}");
            break;
        }
        let mut buf = vec![0u8; frame_len];
        conn.read_exact(&mut buf).await?;

        // Reject old identity layouts before deserializing a request or
        // publishing search hits that an old UI could misinterpret.
        let (kind, payload) = framing::decode_message(&buf)?;
        let response = dispatch(kind, payload);
        anyhow::ensure!(!response.is_empty(), "invalid {kind:?} IPC payload");
        let framed = framing::encode_frame(&framing::encode_message(kind, &response)?)?;
        // framed includes length prefix.
        conn.write_all(&framed).await?;
    }
    Ok(())
}

fn dispatch(kind: framing::MessageKind, payload: &[u8]) -> Vec<u8> {
    fn deserialize_exact<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
        let mut cursor = Cursor::new(payload);
        match bincode::deserialize_from::<_, T>(&mut cursor) {
            Ok(v) if cursor.position() as usize == payload.len() => Some(v),
            _ => None,
        }
    }

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

fn host_label() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "service".into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
