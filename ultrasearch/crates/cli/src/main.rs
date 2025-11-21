use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use console::style;
use ipc::{
    FieldKind, QueryExpr, RangeExpr, RangeOp, RangeValue, SearchMode, SearchRequest, TermExpr,
    TermModifier,
};
use uuid::Uuid;

/// Debug / scripting CLI for UltraSearch IPC.
#[derive(Parser, Debug)]
#[command(name = "ultrasearch-cli", version, about = "UltraSearch debug/diagnostic client")]
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
        /// Search mode (auto/name/content/hybrid).
        #[arg(short, long, value_enum, default_value_t = ModeArg::Auto)]
        mode: ModeArg,
    },
    /// Request service status.
    Status {},
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
        Commands::Search { query, limit, mode } => {
            let req = build_search_request(&query, limit, mode);
            print_request(&req)?;
            send_stub(req)?.map(|resp| {
                println!("{}", style("Response (stubbed):").yellow());
                println!("{resp:#?}");
            })?;
        }
        Commands::Status {} => {
            println!("{}", style("Status request not yet wired to transport.").yellow());
        }
    }
    Ok(())
}

fn build_search_request(query: &str, limit: u32, mode: ModeArg) -> SearchRequest {
    let term = QueryExpr::Term(TermExpr {
        field: None,
        value: query.to_string(),
        modifier: TermModifier::Term,
    });

    SearchRequest {
        id: Uuid::new_v4(),
        query: term,
        limit,
        mode: match mode {
            ModeArg::Auto => SearchMode::Auto,
            ModeArg::Name => SearchMode::NameOnly,
            ModeArg::Content => SearchMode::Content,
            ModeArg::Hybrid => SearchMode::Hybrid,
        },
    }
}

fn print_request(req: &SearchRequest) -> Result<()> {
    println!("{}", style("Sending request (stub transport):").cyan());
    println!("{req:#?}");
    Ok(())
}

/// Placeholder transport: bincode roundtrip to prove serialization.
fn send_stub(req: SearchRequest) -> Result<SearchRequest> {
    let bytes = bincode::serialize(&req)?;
    let back: SearchRequest = bincode::deserialize(&bytes)?;
    Ok(back)
}
