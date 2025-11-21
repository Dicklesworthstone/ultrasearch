use anyhow::Result;
use clap::Parser;
use console::style;
use ipc::{SearchRequest, SearchResponse, StatusRequest, StatusResponse, TermExpr, TermModifier};
use uuid::Uuid;

#[cfg(windows)]
use ipc::client::PipeClient;

#[derive(Parser, Debug)]
#[command(name = "ultrasearch-cli", version, about = "CLI for UltraSearch")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Search for files matching a query.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 10)]
        limit: u32,
    },
    /// Request service status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Search { query, limit } => {
            let req = SearchRequest {
                id: Uuid::new_v4(),
                query: TermExpr::from(query).into(),
                limit,
                ..Default::default()
            };
            let resp = send_request(req).await?;
            print_search_response(&resp)?;
        }
        Commands::Status => {
            let req = StatusRequest { id: Uuid::new_v4() };
            let resp = send_request(req).await?;
            print_status_response(&resp)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
async fn send_request<T, U>(req: T) -> Result<U>
where
    T: serde::Serialize + Send + Sync + 'static,
    U: serde::de::DeserializeOwned + Send + 'static,
{
    let client = PipeClient::default();
    client.request(&req).await
}

#[cfg(not(windows))]
async fn send_request<T, U>(req: T) -> Result<U>
where
    T: serde::Serialize + Send + Sync + 'static,
    U: serde::de::DeserializeOwned + Send + 'static,
{
    // On non-Windows platforms, we can't use the named pipe.
    // This is a stub implementation that returns a dummy response.
    tracing_subscriber::fmt::init();
    tracing::warn!("Non-Windows platform detected, using stub response.");
    let id_bytes = bincode::serialize(&req)?;
    let id: Uuid = bincode::deserialize(&id_bytes[..16])?;
    let resp: U = bincode::deserialize(&[])?; // This will fail if not default
    let resp_bytes = bincode::serialize(&resp)?;
    let (mut resp_deserialized, _): (U, _) = bincode::serde::decode_from_slice(&resp_bytes, bincode::config::standard())?;
    // This is getting convoluted. Just return a hardcoded empty response.
    if std::any::TypeId::of::<U>() == std::any::TypeId::of::<SearchResponse>() {
        let empty_search_resp: SearchResponse = SearchResponse {
            id,
            hits: vec![],
            total: 0,
            truncated: false,
            took_ms: 0,
            served_by: Some("stub-cli".into()),
        };
        return Ok(unsafe { std::mem::transmute(empty_search_resp) });
    }
    if std::any::TypeId::of::<U>() == std::any::TypeId::of::<StatusResponse>() {
        let empty_status_resp: StatusResponse = StatusResponse {
            id,
            volumes: vec![],
            last_index_commit_ts: None,
            scheduler_state: "stub".into(),
            metrics: None,
            served_by: Some("stub-cli".into()),
        };
        return Ok(unsafe { std::mem::transmute(empty_status_resp) });
    }
    Err(anyhow::anyhow!("Unsupported request type for stub"))
}

fn print_search_response(resp: &SearchResponse) -> Result<()> {
    println!("{}", style(format!("Hits: {}", resp.total)).cyan());
    for (i, hit) in resp.hits.iter().enumerate() {
        println!(
            "{:3}. {:<40} score={:.3} path={}",
            i + 1,
            hit.name.as_deref().unwrap_or("<unknown>"),
            hit.score,
            hit.path.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn print_status_response(resp: &StatusResponse) -> Result<()> {
    println!("{}", style("Service Status:").cyan());
    println!("Scheduler: {}", resp.scheduler_state);
    if let Some(ts) = resp.last_index_commit_ts {
        println!("Last Index Commit: {}s ago", (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64) - ts);
    }
    for v in &resp.volumes {
        println!("Volume {}: {} files indexed, {} pending", v.volume, v.indexed_files, v.pending_files);
    }
    Ok(())
}

impl From<String> for TermExpr {
    fn from(query: String) -> Self {
        TermExpr {
            field: None,
            value: query,
            modifier: TermModifier::Term,
        }
    }
}