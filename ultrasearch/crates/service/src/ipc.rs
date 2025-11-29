#![cfg(target_os = "windows")]

use std::env;
use std::time::Instant;

use crate::metrics::{global_metrics_snapshot, record_ipc_request};
use crate::search_handler::search;
use crate::status::make_status_response;
use crate::status_provider::status_snapshot;
use anyhow::Result;
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

/// Start a Tokio named-pipe server that spawns a task per connection.
pub async fn start_pipe_server(pipe_name: Option<&str>) -> Result<JoinHandle<()>> {
    let name = pipe_name.unwrap_or(DEFAULT_PIPE_NAME).to_string();

    let handle = tokio::spawn(async move {
        let mut first = true;
        loop {
            // Use raw Win32 API to create pipe with Security Descriptor
            // SDDL: D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)
            // SY=System, BA=Admins, AU=Authenticated Users (Read/Write)
            let server = match unsafe { create_secure_pipe(&name, first) } {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to create secure named pipe: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };

            first = false;

            if let Err(e) = server.connect().await {
                tracing::error!("named pipe connect failed: {}", e);
                continue;
            }

            tokio::spawn(async move {
                if let Err(e) = handle_connection(server).await {
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

        // framing::decode_frame expects [header + body].
        // We have read them separately.
        // We can reconstruct or just parse the body if we trust it.
        // Since we are the server, we trust our read logic.
        // Dispatch expects the RAW payload (no frame).
        // But wait, `buf` IS the payload.
        // framing::decode_frame also checks length.

        let response = dispatch(&buf);
        let framed = framing::encode_frame(&response).unwrap_or_default();
        // framed includes length prefix.
        conn.write_all(&framed).await?;
    }
    Ok(())
}

fn dispatch(payload: &[u8]) -> Vec<u8> {
    fn deserialize_exact<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
        let mut cursor = Cursor::new(payload);
        match bincode::deserialize_from::<_, T>(&mut cursor) {
            Ok(v) if cursor.position() as usize == payload.len() => Some(v),
            _ => None,
        }
    }

    // Fast-path: ping echo when payload is prefixed with "PING" + UUID.
    if payload.len() >= 20
        && payload.starts_with(b"PING")
        && let Ok(id) = Uuid::from_slice(&payload[4..20])
    {
        return id.as_bytes().to_vec();
    }

    // Try StatusRequest first.
    if let Some(req) = deserialize_exact::<StatusRequest>(payload) {
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
    if let Some(req) = deserialize_exact::<ReloadConfigRequest>(payload) {
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
    if let Some(req) = deserialize_exact::<RescanRequest>(payload) {
        let started = Instant::now();
        let cfg = core_types::config::get_current_config();
        let res = crate::scanner::scan_volumes(&cfg).map(|jobs| {
            let mut submitted = 0usize;
            for job in jobs {
                if crate::scheduler_runtime::enqueue_content_job(job) {
                    submitted += 1;
                }
            }
            submitted
        });

        let (success, message) = match res {
            Ok(count) => (true, Some(format!("Submitted {} jobs", count))),
            Err(e) => (false, Some(e.to_string())),
        };

        let resp = RescanResponse {
            id: req.id,
            success,
            message,
        };
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(started.elapsed());
        return encoded;
    }

    // Fallback: dispatch SearchRequest.
    if let Some(req) = deserialize_exact::<SearchRequest>(payload) {
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
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(elapsed);
        return encoded;
    }
    // If payload decodes as a UUID prefix, echo it back.
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
    async fn echoes_uuid_prefix() {
        let id = Uuid::new_v4();
        let mut payload = b"PING".to_vec();
        payload.extend_from_slice(id.as_bytes());
        let resp = dispatch(&payload);
        assert_eq!(resp, id.as_bytes());
    }

    #[test]
    fn status_request_roundtrip() {
        let req = StatusRequest { id: Uuid::new_v4() };
        let resp_bytes = dispatch(&bincode::serialize(&req).unwrap());
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
        let resp_bytes = dispatch(&bincode::serialize(&req).unwrap());
        let resp: SearchResponse = bincode::deserialize(&resp_bytes).unwrap();
        assert_eq!(resp.id, req.id);
        assert!(resp.hits.is_empty());
        assert_eq!(resp.total, 0);
    }
}
