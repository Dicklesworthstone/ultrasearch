//! Entry point for the UltraSearch Windows service (bootstrap only for now).

use anyhow::Result;
use core_types::config::load_config;
use service::{init_tracing, metrics::init_metrics_from_config};

fn main() -> Result<()> {
    let cfg = load_config(None)?;
    init_tracing()?;

    if cfg.metrics.enabled {
        let _metrics = init_metrics_from_config(&cfg.metrics)?;
        // TODO: wire metrics handle into IPC/server once implemented.
    }

    println!("UltraSearch service placeholder – wiring pending.");

    Ok(())
}
