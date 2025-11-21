#![cfg(target_os = "windows")]

use std::env;
use std::time::Instant;

use anyhow::Result;
use ipc::{
    framing, MetricsSnapshot, SearchRequest, SearchResponse, StatusRequest, StatusResponse,
};
use service::metrics::{global_metrics_snapshot, record_ipc_request};
use service::search_handler::search;
use service::status::make_status_response;
use service::status_provider::status_snapshot;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
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
            let server_res = ServerOptions::new()
                .first_pipe_instance(first)
                .create(&name);
            
            let mut server = match server_res {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("failed to create named pipe instance: {}", e);
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
                    tracing::warn!("pipe connection error: {{:?}}", e);
                }
            });
        }
    });

    Ok(handle)
}

async fn handle_connection(mut conn: NamedPipeServer) -> Result<()>
{
    loop {
        // decode frame
        let mut len_prefix = [0u8; 4];
        // If read returns 0, client disconnected (or EOF).
        if conn.read_exact(&mut len_prefix).await.is_err() {
            break;
        }
        let frame_len = u32::from_le_bytes(len_prefix) as usize;
        if frame_len == 0 || frame_len > MAX_MESSAGE_BYTES {
            tracing::warn!("invalid frame size {{}}", frame_len);
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
    // Try StatusRequest first.
    if let Ok(req) = bincode::deserialize::<StatusRequest>(payload) {
        let started = Instant::now();
        let snap = status_snapshot();
        let empty_metrics = snap.metrics.or_else(|| {
            global_metrics_snapshot(Some(0), Some(0)).or_else(|| {
                Some(MetricsSnapshot {
                    search_latency_ms_p50: None,
                    search_latency_ms_p95: None,
                    worker_cpu_pct: None,
                    worker_mem_bytes: None,
                    queue_depth: Some(0),
                    active_workers: Some(0),
                })
            })
        });
        let resp = make_status_response(
            req.id,
            snap.volumes,
            snap.scheduler_state,
            empty_metrics,
            snap.last_index_commit_ts,
        );
        let encoded = bincode::serialize(&resp).unwrap_or_default();
        record_ipc_request(started.elapsed());
        return encoded;
    }
    // Fallback: dispatch SearchRequest.
    if let Ok(req) = bincode::deserialize::<SearchRequest>(payload) {
        let start = Instant::now();
        let mut resp = search(req);
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
    if payload.len() >= 16 {
        if let Ok(id) = Uuid::from_slice(&payload[..16]) {
            return id.as_bytes().to_vec();
        }
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
    use service::status::make_status_response;

    #[tokio::test]
    async fn echoes_uuid_prefix() {
        let id = Uuid::new_v4();
        let resp = dispatch(id.as_bytes());
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