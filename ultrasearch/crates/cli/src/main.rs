use clap::Parser;

/// Minimal debug CLI placeholder.
#[derive(Parser, Debug)]
#[command(name = "ultrasearch-cli", version, about = "UltraSearch debug client (placeholder)")]
struct Args {
    /// Query string to send (informational only for now).
    query: Option<String>,
}

fn main() {
    let args = Args::parse();
    if let Some(q) = args.query {
        println!("CLI placeholder; would run search for: {}", q);
    } else {
        println!("CLI placeholder; no query provided.");
    }
}
