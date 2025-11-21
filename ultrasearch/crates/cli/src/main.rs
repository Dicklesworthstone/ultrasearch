use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use console::style;
use ipc::framing;
use ipc::{QueryExpr, SearchMode, SearchRequest, SearchResponse, TermExpr, TermModifier};
use ipc::{StatusRequest, StatusResponse};
use serde_json::to_string_pretty;
use uuid::Uuid;

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
    /// Run a search query (IPC transport to be wired later).
    Search {
        /// Query string (simple term; planner will expand later).
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
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Request service status.
    Status {
        /// Output as JSON
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

fn main() -> Result<()> {
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
            print_request(&req)?;
            let resp = send_stub(req)?;
            if json {
                println!("{}", to_string_pretty(&resp)?);
            } else {
                print_search_response(&resp)?;
            }
        }
        Commands::Status { json } => {
            let req = build_status_request();
            println!("{}", style("Sending status request (stub framing):").cyan());
            let framed = framing::encode_frame(&bincode::serialize(&req)?)?;
            let (_payload, _rem) = framing::decode_frame(&framed)?;
            // Stub: fabricate an empty response
            let resp = StatusResponse {
                id: req.id,
                volumes: vec![],
                last_index_commit_ts: None,
                scheduler_state: "unknown".into(),
                metrics: None,
            };
            if json {
                println!("{}", to_string_pretty(&resp)?);
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

fn print_request(req: &SearchRequest) -> Result<()> {
    println!("{}", style("Sending request (stub transport):").cyan());
    println!("{req:#?}");
    Ok(())
}

fn build_status_request() -> StatusRequest {
    StatusRequest { id: Uuid::new_v4() }
}

fn print_status_response(resp: &StatusResponse) -> Result<()> {
    println!("{}", style("Status response (stubbed):").yellow());
    println!("Scheduler: {}", resp.scheduler_state);
    println!("Volumes: {}", resp.volumes.len());
    for v in &resp.volumes {
        println!(
            "- vol {:02} indexed {} pending {}",
            v.volume, v.indexed_files, v.pending_files
        );
    }
    Ok(())
}

fn print_search_response(resp: &SearchResponse) -> Result<()> {
    println!("{}", style("Hits:").cyan());
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
            "shown {} / total {} (truncated: {}) took_ms: {}",
            resp.hits.len(),
            resp.total,
            resp.truncated,
            resp.took_ms
        ))
        .green()
    );
    Ok(())
}

/// Placeholder transport: bincode roundtrip and fabricate an empty response.
fn send_stub(req: SearchRequest) -> Result<SearchResponse> {
    let bytes = bincode::serialize(&req)?;
    let framed = framing::encode_frame(&bytes)?;
    let (payload, _rem) = framing::decode_frame(&framed)?;
    let _back: SearchRequest = bincode::deserialize(&payload)?;

    Ok(SearchResponse {
        id: req.id,
        hits: Vec::new(),
        total: 0,
        truncated: false,
        took_ms: 0,
    })
}
