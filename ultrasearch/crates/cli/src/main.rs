use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use console::style;
use core_types::config::{default_config_path, load_or_create_config};
#[cfg(not(windows))]
use ipc::MetricsSnapshot;
use ipc::{
    QueryExpr, ReloadConfigRequest, RescanRequest, SearchMode, SearchRequest, SearchResponse,
    StatusRequest, StatusResponse, TermExpr, TermModifier,
};
use uuid::Uuid;

#[cfg(windows)]
use ipc::client::PipeClient;

/// UltraSearch CLI — Typer-style, self-documenting commands for agents and humans.
#[derive(Parser, Debug)]
#[command(
    name = "ultrasearch",
    version,
    about = "UltraSearch command-line client"
)]
struct Cli {
    /// Override pipe name (default: \\.\pipe\ultrasearch)
    #[arg(long)]
    pipe: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a search query over IPC.
    Search {
        /// Query string (full-text or filename).
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

    /// Request service status (volumes, queues, metrics).
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Ask the service to reload its config file.
    ReloadConfig {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Ask the service to rescan volumes and enqueue indexing jobs.
    Rescan {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show or edit the config on disk (ProgramData).
    Config {
        #[command(subcommand)]
        sub: ConfigCmd,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the effective config path and contents.
    Show {
        /// Output as JSON (raw TOML otherwise).
        #[arg(long)]
        json: bool,
    },
    /// Set volumes and content-index volumes in the config file.
    SetVolumes {
        /// Volumes to include (e.g., C:\ D:\). If omitted, defaults to all discovered NTFS volumes.
        #[arg(long, num_args = 0..)]
        volume: Vec<String>,
        /// Volumes to content-index (subset). If omitted, mirrors --volume.
        #[arg(long, num_args = 0..)]
        content_volume: Vec<String>,
        /// Output resulting config as JSON.
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
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    match cli.command {
        Commands::Search {
            ref query,
            limit,
            offset,
            mode,
            timeout_ms,
            json,
        } => {
            let req = build_search_request(query, limit, offset, timeout_ms, mode);
            let resp = pipe(&cli).search(req).await?;
            output(resp, json, print_search_response)?;
        }
        Commands::Status { json } => {
            let req = StatusRequest { id: Uuid::new_v4() };
            let resp = pipe(&cli).status(req).await?;
            output(resp, json, print_status_response)?;
        }
        Commands::ReloadConfig { json } => {
            let req = ReloadConfigRequest { id: Uuid::new_v4() };
            let resp = pipe(&cli).reload_config(req).await?;
            output(resp, json, |r| {
                println!(
                    "{} {}",
                    style("Reload config:").green(),
                    if r.success { "ok" } else { "failed" }
                );
                if let Some(msg) = &r.message {
                    println!("  {}", msg);
                }
                Ok(())
            })?;
        }
        Commands::Rescan { json } => {
            let req = RescanRequest { id: Uuid::new_v4() };
            let resp = pipe(&cli).rescan(req).await?;
            output(resp, json, |r| {
                println!(
                    "{} {}",
                    style("Rescan:").green(),
                    if r.success { "ok" } else { "failed" }
                );
                if let Some(msg) = &r.message {
                    println!("  {}", msg);
                }
                Ok(())
            })?;
        }
        Commands::Config { sub } => match sub {
            ConfigCmd::Show { json } => {
                let path = default_config_path();
                let cfg = load_or_create_config(None)?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "path": path,
                            "config": cfg,
                        })
                    );
                } else {
                    println!("{}", style("Config path:").green());
                    println!("  {}", path.to_string_lossy());
                    println!("{}", style("Config:").green());
                    let toml = toml::to_string_pretty(&cfg)?;
                    println!("{toml}");
                }
            }
            ConfigCmd::SetVolumes {
                volume,
                content_volume,
                json,
            } => {
                let mut cfg = load_or_create_config(None)?;
                let vols = if volume.is_empty() {
                    cfg.volumes.clone()
                } else {
                    volume
                };
                let content = if content_volume.is_empty() {
                    if vols.is_empty() {
                        Vec::new()
                    } else {
                        vols.clone()
                    }
                } else {
                    content_volume
                };
                cfg.volumes = vols;
                cfg.content_index_volumes = content;
                let path = default_config_path();
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let toml = toml::to_string_pretty(&cfg)?;
                std::fs::write(&path, toml)?;
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "path": path,
                            "config": cfg,
                        })
                    );
                } else {
                    println!("{}", style("Updated config").green());
                    println!("  {}", path.to_string_lossy());
                }
            }
        },
    }
    Ok(())
}

#[cfg(windows)]
fn pipe(cli: &Cli) -> PipeClient {
    cli.pipe
        .as_ref()
        .map(|p| PipeClient::new(p.clone()))
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn pipe(_cli: &Cli) -> StubClient {
    StubClient
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
        if let Some(enq) = metrics.content_enqueued {
            println!("    Content Jobs Enqueued: {}", enq);
        }
        if let Some(drop) = metrics.content_dropped {
            println!("    Content Jobs Dropped: {}", drop);
        }
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

fn output<T, F>(value: T, json: bool, pretty: F) -> Result<()>
where
    T: serde::Serialize,
    F: FnOnce(&T) -> Result<()>,
{
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        pretty(&value)?;
    }
    Ok(())
}

#[cfg(not(windows))]
struct StubClient;

#[cfg(not(windows))]
impl StubClient {
    async fn status(&self, _: StatusRequest) -> Result<StatusResponse> {
        stub_status(StatusRequest { id: Uuid::new_v4() }).await
    }
    async fn search(&self, req: SearchRequest) -> Result<SearchResponse> {
        stub_search(req).await
    }
    async fn reload_config(&self, _: ReloadConfigRequest) -> Result<ipc::ReloadConfigResponse> {
        Ok(ipc::ReloadConfigResponse {
            id: Uuid::new_v4(),
            success: true,
            message: Some("stub".into()),
        })
    }
    async fn rescan(&self, _: RescanRequest) -> Result<ipc::RescanResponse> {
        Ok(ipc::RescanResponse {
            id: Uuid::new_v4(),
            success: true,
            message: Some("stub".into()),
        })
    }
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
            content_enqueued: Some(0),
            content_dropped: Some(0),
        }),
        served_by: Some("cli-linux-stub".into()),
    })
}
