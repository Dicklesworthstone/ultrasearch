use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use console::style;
use core_types::config::{default_config_path, load_or_create_config};
use ipc::{
    FieldKind, QueryExpr, RangeExpr, RangeOp, RangeValue, ReloadConfigRequest, RescanRequest,
    SearchMode, SearchRequest, SearchResponse, StatusRequest, StatusResponse, TermExpr,
    TermModifier,
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
        #[command(flatten)]
        query: SearchQueryArgs,
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

    /// Ping the service over IPC and report round-trip latency.
    Ping {
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

#[derive(Args, Debug, Default)]
struct SearchQueryArgs {
    /// Raw query syntax, or literal text with --prefix, --phrase, or --fuzzy.
    #[arg(
        value_name = "QUERY",
        required_unless_present_any = [
            "min_size", "max_size", "modified_since", "modified_through"
        ]
    )]
    query: Option<String>,
    /// Default field for raw syntax; target field for literal matching.
    #[arg(long, value_enum)]
    field: Option<SearchFieldArg>,
    /// Match every analyzed query word as a literal prefix.
    #[arg(long, conflicts_with_all = ["phrase", "fuzzy"])]
    prefix: bool,
    /// Match literal query words in order at their analyzed positions.
    #[arg(long, conflicts_with_all = ["prefix", "fuzzy"])]
    phrase: bool,
    /// Match each query word within 0–2 edits; a transposition costs one.
    #[arg(
        long,
        value_name = "EDITS",
        value_parser = clap::value_parser!(u8).range(0..=2),
        conflicts_with_all = ["prefix", "phrase"]
    )]
    fuzzy: Option<u8>,
    /// Inclusive minimum file size in bytes.
    #[arg(long, value_name = "BYTES")]
    min_size: Option<u64>,
    /// Inclusive maximum file size in bytes.
    #[arg(long, value_name = "BYTES")]
    max_size: Option<u64>,
    /// Inclusive earliest modification time, as Unix seconds.
    #[arg(long, value_name = "UNIX_SECONDS", allow_negative_numbers = true)]
    modified_since: Option<i64>,
    /// Inclusive latest modification time, as Unix seconds.
    #[arg(long, value_name = "UNIX_SECONDS", allow_negative_numbers = true)]
    modified_through: Option<i64>,
}

impl SearchQueryArgs {
    fn query_expr(&self) -> Result<QueryExpr> {
        let text = self.query.as_deref().filter(|text| !text.trim().is_empty());
        ensure!(
            text.is_some()
                || (self.field.is_none() && !self.prefix && !self.phrase && self.fuzzy.is_none()),
            "--field and literal matching options require nonempty query text"
        );
        let mut clauses = Vec::with_capacity(3);
        if let Some(text) = text {
            let has_explicit_modifier =
                self.field.is_some() || self.prefix || self.phrase || self.fuzzy.is_some();
            if !has_explicit_modifier && ipc::has_human_filter_tokens(text) {
                clauses.push(ipc::parse_query_str(text));
            } else {
                let modifier = if self.prefix {
                    TermModifier::Prefix
                } else if self.phrase {
                    TermModifier::Phrase
                } else if let Some(distance) = self.fuzzy {
                    TermModifier::Fuzzy(distance)
                } else {
                    TermModifier::Term
                };
                clauses.push(QueryExpr::Term(TermExpr {
                    field: self.field.map(|field| match field {
                        SearchFieldArg::Name => FieldKind::Name,
                        SearchFieldArg::Path => FieldKind::Path,
                        SearchFieldArg::Ext => FieldKind::Ext,
                        SearchFieldArg::Content => FieldKind::Content,
                    }),
                    // Preserve the exact raw expression, including whitespace and
                    // embedded field qualifiers. Literal interpretation is selected
                    // only by the explicit modifier flags above.
                    value: text.to_string(),
                    modifier,
                }));
            }
        }
        if let Some(size) =
            inclusive_filter(FieldKind::Size, self.min_size, self.max_size, |lo, hi| {
                RangeValue::U64 { lo, hi }
            })?
        {
            clauses.push(size);
        }
        if let Some(modified) = inclusive_filter(
            FieldKind::Modified,
            self.modified_since,
            self.modified_through,
            |lo, hi| RangeValue::I64 { lo, hi },
        )? {
            clauses.push(modified);
        }
        match clauses.len() {
            0 => bail!(
                "provide nonempty query text or at least one size or modification-time filter"
            ),
            1 => clauses.pop().context("search criterion is missing"),
            _ => Ok(QueryExpr::And(clauses)),
        }
    }
}

fn inclusive_filter<T: Ord>(
    field: FieldKind,
    minimum: Option<T>,
    maximum: Option<T>,
    value: impl FnOnce(T, Option<T>) -> RangeValue,
) -> Result<Option<QueryExpr>> {
    let (op, lo, hi) = match (minimum, maximum) {
        (None, None) => return Ok(None),
        (Some(lo), None) => (RangeOp::Ge, lo, None),
        (None, Some(hi)) => (RangeOp::Le, hi, None),
        (Some(lo), Some(hi)) => {
            ensure!(lo <= hi, "{field:?} minimum exceeds its maximum");
            (RangeOp::Between, lo, Some(hi))
        }
    };
    Ok(Some(QueryExpr::Range(RangeExpr {
        field,
        op,
        value: value(lo, hi),
    })))
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SearchFieldArg {
    Name,
    Path,
    Ext,
    Content,
}

#[derive(Subcommand, Debug, Clone)]
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
        /// Immediately ask the running service to reload config and rescan over IPC.
        #[arg(long)]
        reload: bool,
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
            let req = build_search_request(query, limit, offset, timeout_ms, mode)?;
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
        Commands::Ping { json } => {
            let rtt = pipe(&cli).ping().await?;
            let rtt_ms = rtt.as_secs_f64() * 1000.0;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "rtt_ms": rtt_ms,
                    })
                );
            } else {
                println!("{} {:.2} ms", style("Pong:").green(), rtt_ms);
            }
        }
        Commands::Config { ref sub } => match sub.clone() {
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
                reload,
            } => {
                let mut cfg = load_or_create_config(None)?;
                let vols = if volume.is_empty() {
                    let discovered: Vec<String> = ntfs_watcher::discover_volumes()
                        .unwrap_or_default()
                        .into_iter()
                        .flat_map(|v| v.drive_letters.into_iter().map(|l| format!("{l}:\\")))
                        .collect();
                    if discovered.is_empty() {
                        cfg.volumes.clone()
                    } else {
                        discovered
                    }
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
                if reload {
                    let client = pipe(&cli);
                    let _ = client
                        .reload_config(ReloadConfigRequest { id: Uuid::new_v4() })
                        .await?;
                    let _ = client.rescan(RescanRequest { id: Uuid::new_v4() }).await?;
                }
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "path": path,
                            "config": cfg,
                            "reloaded": reload,
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
fn pipe(_cli: &Cli) -> UnsupportedClient {
    UnsupportedClient
}

fn build_search_request(
    query: &SearchQueryArgs,
    limit: u32,
    offset: u32,
    timeout_ms: Option<u64>,
    mode: ModeArg,
) -> Result<SearchRequest> {
    let query = query.query_expr()?;
    Ok(SearchRequest {
        id: Uuid::new_v4(),
        query,
        limit,
        offset,
        mode: match mode {
            ModeArg::Auto => SearchMode::Auto,
            ModeArg::Name => SearchMode::NameOnly,
            ModeArg::Content => SearchMode::Content,
            ModeArg::Hybrid => SearchMode::Hybrid,
        },
        timeout: timeout_ms.map(std::time::Duration::from_millis),
    })
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
struct UnsupportedClient;

#[cfg(not(windows))]
impl UnsupportedClient {
    async fn status(&self, _: StatusRequest) -> Result<StatusResponse> {
        unsupported_service()
    }
    async fn search(&self, _: SearchRequest) -> Result<SearchResponse> {
        unsupported_service()
    }
    async fn reload_config(&self, _: ReloadConfigRequest) -> Result<ipc::ReloadConfigResponse> {
        unsupported_service()
    }
    async fn rescan(&self, _: RescanRequest) -> Result<ipc::RescanResponse> {
        unsupported_service()
    }
    async fn ping(&self) -> Result<std::time::Duration> {
        unsupported_service()
    }
}

#[cfg(not(windows))]
fn unsupported_service<T>() -> Result<T> {
    anyhow::bail!(
        "UltraSearch service commands require Windows named-pipe IPC; run this command on Windows with the UltraSearch service installed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_search(args: &[&str]) -> Result<SearchRequest> {
        let cli = Cli::try_parse_from(
            ["ultrasearch", "search"]
                .into_iter()
                .chain(args.iter().copied()),
        )?;
        let Commands::Search {
            query,
            limit,
            offset,
            mode,
            timeout_ms,
            ..
        } = cli.command
        else {
            bail!("expected search command");
        };
        build_search_request(&query, limit, offset, timeout_ms, mode)
    }

    #[test]
    fn raw_search_preserves_syntax_and_request_controls() -> Result<()> {
        let raw = "  name:alpha OR content:\"two words\"  ";
        let request = parse_search(&[
            raw,
            "--limit",
            "7",
            "--offset",
            "3",
            "--mode",
            "content",
            "--timeout-ms",
            "4500",
            "--json",
        ])?;
        assert_eq!(
            request.query,
            QueryExpr::Term(TermExpr {
                field: None,
                value: raw.to_string(),
                modifier: TermModifier::Term,
            })
        );
        assert!(!request.id.is_nil());
        assert_eq!(request.limit, 7);
        assert_eq!(request.offset, 3);
        assert_eq!(request.mode, SearchMode::Content);
        assert_eq!(
            request.timeout,
            Some(std::time::Duration::from_millis(4500))
        );

        // A field remains the raw parser's default, without escaping or
        // rewriting explicit field qualifiers embedded in the expression.
        let request = parse_search(&[raw, "--field", "path"])?;
        assert_eq!(
            request.query,
            QueryExpr::Term(TermExpr {
                field: Some(FieldKind::Path),
                value: raw.to_string(),
                modifier: TermModifier::Term,
            })
        );
        Ok(())
    }

    #[test]
    fn literal_flags_and_fields_build_exact_term_modifiers() -> Result<()> {
        let modes: &[(&[&str], TermModifier)] = &[
            (&["--prefix"], TermModifier::Prefix),
            (&["--phrase"], TermModifier::Phrase),
            (&["--fuzzy", "0"], TermModifier::Fuzzy(0)),
            (&["--fuzzy", "1"], TermModifier::Fuzzy(1)),
            (&["--fuzzy", "2"], TermModifier::Fuzzy(2)),
        ];
        for (field, expected_field) in [
            ("name", FieldKind::Name),
            ("path", FieldKind::Path),
            ("ext", FieldKind::Ext),
            ("content", FieldKind::Content),
        ] {
            for &(flags, modifier) in modes {
                let mut args = vec!["  NAME:alpha OR beta  ", "--field", field];
                args.extend_from_slice(flags);
                let request = parse_search(&args)?;
                assert_eq!(
                    request.query,
                    QueryExpr::Term(TermExpr {
                        field: Some(expected_field),
                        value: "  NAME:alpha OR beta  ".to_string(),
                        modifier,
                    })
                );
            }
        }
        let request = parse_search(&["report", "--prefix"])?;
        assert_eq!(
            request.query,
            QueryExpr::Term(TermExpr {
                field: None,
                value: "report".to_string(),
                modifier: TermModifier::Prefix,
            })
        );
        Ok(())
    }

    #[test]
    fn typed_filters_allow_filter_only_searches_without_empty_boolean_clauses() -> Result<()> {
        let cases: &[(&[&str], RangeExpr)] = &[
            (
                &["--min-size", "1024"],
                RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Ge,
                    value: RangeValue::U64 { lo: 1024, hi: None },
                },
            ),
            (
                &["--max-size", "18446744073709551615"],
                RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Le,
                    value: RangeValue::U64 {
                        lo: u64::MAX,
                        hi: None,
                    },
                },
            ),
            (
                &["--min-size", "0", "--max-size", "18446744073709551615"],
                RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Between,
                    value: RangeValue::U64 {
                        lo: 0,
                        hi: Some(u64::MAX),
                    },
                },
            ),
            (
                &["--min-size", "0", "--max-size", "0"],
                RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Between,
                    value: RangeValue::U64 { lo: 0, hi: Some(0) },
                },
            ),
            (
                &["--modified-since", "-1"],
                RangeExpr {
                    field: FieldKind::Modified,
                    op: RangeOp::Ge,
                    value: RangeValue::I64 { lo: -1, hi: None },
                },
            ),
            (
                &["--modified-through", "0"],
                RangeExpr {
                    field: FieldKind::Modified,
                    op: RangeOp::Le,
                    value: RangeValue::I64 { lo: 0, hi: None },
                },
            ),
            (
                &[
                    "--modified-since",
                    "-9223372036854775808",
                    "--modified-through",
                    "9223372036854775807",
                ],
                RangeExpr {
                    field: FieldKind::Modified,
                    op: RangeOp::Between,
                    value: RangeValue::I64 {
                        lo: i64::MIN,
                        hi: Some(i64::MAX),
                    },
                },
            ),
            (
                &["  ", "--min-size", "0"],
                RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Ge,
                    value: RangeValue::U64 { lo: 0, hi: None },
                },
            ),
        ];
        for (args, expected) in cases {
            let request = parse_search(args)?;
            assert_eq!(
                request.query,
                QueryExpr::Range(expected.clone()),
                "{args:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn literal_and_range_filters_survive_the_versioned_ipc_wire() -> Result<()> {
        let request = parse_search(&[
            "mystery OR \"case\"",
            "--field",
            "content",
            "--phrase",
            "--min-size",
            "1024",
            "--max-size",
            "18446744073709551615",
            "--modified-since",
            "-1",
            "--modified-through",
            "1700000000",
            "--mode",
            "hybrid",
            "--limit",
            "13",
            "--offset",
            "7",
            "--timeout-ms",
            "999",
        ])?;
        let payload = bincode::serialize(&request)?;
        let message = ipc::framing::encode_message(ipc::framing::MessageKind::Search, &payload)?;
        let frame = ipc::framing::encode_frame(&message)?;
        let (message, remainder) = ipc::framing::decode_frame(&frame)?;
        assert!(remainder.is_empty());
        let (kind, payload) = ipc::framing::decode_message(&message)?;
        assert_eq!(kind, ipc::framing::MessageKind::Search);
        let decoded: SearchRequest = bincode::deserialize(payload)?;
        assert_eq!(
            decoded.query,
            QueryExpr::And(vec![
                QueryExpr::Term(TermExpr {
                    field: Some(FieldKind::Content),
                    value: "mystery OR \"case\"".to_string(),
                    modifier: TermModifier::Phrase,
                }),
                QueryExpr::Range(RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Between,
                    value: RangeValue::U64 {
                        lo: 1024,
                        hi: Some(u64::MAX)
                    },
                }),
                QueryExpr::Range(RangeExpr {
                    field: FieldKind::Modified,
                    op: RangeOp::Between,
                    value: RangeValue::I64 {
                        lo: -1,
                        hi: Some(1_700_000_000)
                    },
                }),
            ])
        );
        assert_eq!(decoded.id, request.id);
        assert_eq!(decoded.mode, SearchMode::Hybrid);
        assert_eq!(decoded.limit, 13);
        assert_eq!(decoded.offset, 7);
        assert_eq!(decoded.timeout, Some(std::time::Duration::from_millis(999)));
        Ok(())
    }

    #[test]
    fn cli_parser_rejects_conflicting_modifiers_and_invalid_typed_values() {
        let invalid: &[&[&str]] = &[
            &["report", "--prefix", "--phrase"],
            &["report", "--prefix", "--fuzzy", "1"],
            &["report", "--phrase", "--fuzzy", "2"],
            &["report", "--fuzzy", "3"],
            &["report", "--fuzzy", "255"],
            &["report", "--fuzzy", "-1"],
            &["report", "--field", "size"],
            &["--min-size", "-1"],
            &["--max-size", "18446744073709551616"],
            &["--modified-since", "-9223372036854775809"],
            &["--modified-through", "9223372036854775808"],
            &["--field", "name"],
            &["--prefix"],
            &[],
        ];
        for args in invalid {
            let parsed = Cli::try_parse_from(
                ["ultrasearch", "search"]
                    .into_iter()
                    .chain(args.iter().copied()),
            );
            assert!(parsed.is_err(), "unexpectedly accepted {args:?}");
        }
    }

    #[test]
    fn query_builder_rejects_empty_criteria_and_reversed_ranges_before_ipc() {
        let invalid: &[(&[&str], &str)] = &[
            (&["  "], "provide nonempty query text"),
            (&["", "--prefix"], "require nonempty query text"),
            (
                &["", "--phrase", "--min-size", "0"],
                "require nonempty query text",
            ),
            (
                &["--fuzzy", "0", "--max-size", "10"],
                "require nonempty query text",
            ),
            (
                &["--field", "name", "--min-size", "0"],
                "require nonempty query text",
            ),
            (
                &["report", "--min-size", "2", "--max-size", "1"],
                "Size minimum exceeds",
            ),
            (
                &["--modified-since", "1", "--modified-through", "-1"],
                "Modified minimum exceeds",
            ),
        ];
        for (args, expected_error) in invalid {
            let failure = parse_search(args).unwrap_err().to_string();
            assert!(failure.contains(expected_error), "{args:?}: {failure}");
        }
        assert!(SearchQueryArgs::default().query_expr().is_err());
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn unsupported_service_operations_cannot_report_success() {
        let client = UnsupportedClient;
        let failures = [
            client.search(SearchRequest::default()).await.unwrap_err(),
            client
                .status(StatusRequest { id: Uuid::new_v4() })
                .await
                .unwrap_err(),
            client
                .reload_config(ReloadConfigRequest { id: Uuid::new_v4() })
                .await
                .unwrap_err(),
            client
                .rescan(RescanRequest { id: Uuid::new_v4() })
                .await
                .unwrap_err(),
        ];
        for failure in failures {
            assert!(
                failure
                    .to_string()
                    .contains("require Windows named-pipe IPC")
            );
        }
    }

    #[test]
    fn human_filter_tokens_build_structured_query_ast_in_cli() -> Result<()> {
        let request = parse_search(&["ext:rs kind:code size:>1kb report"])?;
        assert_eq!(
            request.query,
            QueryExpr::And(vec![
                QueryExpr::Term(TermExpr {
                    field: Some(FieldKind::Ext),
                    value: "rs".into(),
                    modifier: TermModifier::Term,
                }),
                QueryExpr::Term(TermExpr {
                    field: Some(FieldKind::Kind),
                    value: "code".into(),
                    modifier: TermModifier::Term,
                }),
                QueryExpr::Range(RangeExpr {
                    field: FieldKind::Size,
                    op: RangeOp::Gt,
                    value: RangeValue::U64 { lo: 1024, hi: None },
                }),
                QueryExpr::Term(TermExpr {
                    field: None,
                    value: "report".into(),
                    modifier: TermModifier::Term,
                }),
            ])
        );
        Ok(())
    }
}
