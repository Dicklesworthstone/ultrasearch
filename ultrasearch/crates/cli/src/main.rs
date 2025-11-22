use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use console::style;
#[cfg(not(windows))]
use ipc::MetricsSnapshot;
use ipc::{
    QueryExpr, SearchMode, SearchRequest, SearchResponse, StatusRequest, StatusResponse, TermExpr,
    TermModifier,
};
use uuid::Uuid;

#[cfg(windows)]
use ipc::client::PipeClient;

/// Debug / scripting CLI for UltraSearch IPC.
#[derive(Parser, Debug)]
#[command(
    name = "ultrasearch-cli",
    version,
    about = "UltraSearch debug/diagnostic client"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a search query (IPC transport).
    Search {
        /// Query string.
        query: String,
        /// Limit results.
        #[arg(short, long, default_value_t = 20)]
        limit: u32,
        /// Offset for pagination.
        #[arg(short = 'o', long, default_value_t = 0)]
        offset: u32,
        /// Search mode (auto/name/content/hybrid).
        #[arg(short, long, value_enum, default_value_t = ModeArg::Auto)]
        mode: ModeArg,
        /// Optional timeout in milliseconds.
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Request service status.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum ModeArg {
    Auto,
    Name,
    Content,
    Hybrid,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Search {
            query,
            limit,
            offset,
            mode,
            timeout_ms,
            json,
        } => {
            let req = build_search_request(&query, limit, offset, timeout_ms, mode);
            if !json {
                println!("{}", style("Sending request...").cyan());
            }

            #[cfg(windows)]
            let resp = PipeClient::default().search(req).await?;

            #[cfg(not(windows))]
            let resp = stub_search(req).await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_search_response(&resp)?;
            }
        }
        Commands::Status { json } => {
            let req = StatusRequest { id: Uuid::new_v4() };

            #[cfg(windows)]
            let resp = PipeClient::default().status(req).await?;

            #[cfg(not(windows))]
            let resp = stub_status(req).await?;

            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                print_status_response(&resp)?;
            }
        }
    }
    Ok(())
}

fn build_search_request(
    query: &str,
    limit: u32,
    offset: u32,
    timeout_ms: Option<u64>,
    mode: ModeArg,
) -> SearchRequest {
    let term = QueryExpr::Term(TermExpr {
        field: None,
        value: query.to_string(),
        modifier: TermModifier::Term,
    });

    SearchRequest {
        id: Uuid::new_v4(),
        query: term,
        limit,
        offset,
        mode: match mode {
            ModeArg::Auto => SearchMode::Auto,
            ModeArg::Name => SearchMode::NameOnly,
            ModeArg::Content => SearchMode::Content,
            ModeArg::Hybrid => SearchMode::Hybrid,
        },
        timeout: timeout_ms.map(std::time::Duration::from_millis),
    }
}

fn print_status_response(resp: &StatusResponse) -> Result<()> {
    println!("{}", style("Service Status:").green());
    println!("  Scheduler: {}", resp.scheduler_state);
    println!(
        "  Served By: {}",
        resp.served_by.as_deref().unwrap_or("unknown")
    );

    if let Some(metrics) = &resp.metrics {
        println!("{}", style("Metrics:").yellow());
        println!("    Queue Depth: {}", metrics.queue_depth.unwrap_or(0));
        println!(
            "    Active Workers: {}",
            metrics.active_workers.unwrap_or(0)
        );
    }

    println!(
        "{}",
        style(format!("Volumes: {}", resp.volumes.len())).yellow()
    );
    for v in &resp.volumes {
        println!(
            "    Vol {:02}: Indexed {} | Pending {}",
            v.volume, v.indexed_files, v.pending_files
        );
    }
    Ok(())
}

fn print_search_response(resp: &SearchResponse) -> Result<()> {
    println!("{}", style("Hits:").green());
    for (i, hit) in resp.hits.iter().enumerate() {
        println!(
            "{:3}. {:<40} {:<6} score={:.3} path={}",
            i + 1,
            hit.name.as_deref().unwrap_or("<unknown>"),
            hit.ext.as_deref().unwrap_or(""),
            hit.score,
            hit.path.as_deref().unwrap_or("")
        );
    }
    println!(
        "{}",
        style(format!(
            "Shown {} / Total {} (Truncated: {}) Took: {}ms",
            resp.hits.len(),
            resp.total,
            resp.truncated,
            resp.took_ms
        ))
        .dim()
    );
    Ok(())
}

#[cfg(not(windows))]
async fn stub_search(req: SearchRequest) -> Result<SearchResponse> {
    println!(
        "{}",
        style("Warning: Running on non-Windows (stub mode)").red()
    );
    Ok(SearchResponse {
        id: req.id,
        hits: Vec::new(),
        total: 0,
        truncated: false,
        took_ms: 0,
        served_by: Some("cli-linux-stub".into()),
    })
}

#[cfg(not(windows))]
async fn stub_status(req: StatusRequest) -> Result<StatusResponse> {
    println!(
        "{}",
        style("Warning: Running on non-Windows (stub mode)").red()
    );
    Ok(StatusResponse {
        id: req.id,
        volumes: vec![],
        last_index_commit_ts: None,
        scheduler_state: "stubbed".into(),
        metrics: Some(MetricsSnapshot {
            search_latency_ms_p50: None,
            search_latency_ms_p95: None,
            worker_cpu_pct: None,
            worker_mem_bytes: None,
            queue_depth: Some(0),
            active_workers: Some(0),
        }),
        served_by: Some("cli-linux-stub".into()),
    })
}
