//! Entry point for the UltraSearch Windows service (bootstrap only for now).

use std::{path::Path, sync::Arc, thread, time::Duration};

use anyhow::Result;
use core_types::config::load_or_create_config;
use scheduler::SchedulerConfig;
use service::{
    init_tracing_with_config,
    metrics::{init_metrics_from_config, set_global_metrics},
    scheduler_runtime::SchedulerRuntime,
    search_handler::{MetaIndexSearchHandler, set_search_handler},
    status_provider::init_basic_status_provider,
};

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cfg = load_or_create_config(None)?;
    let _guard = init_tracing_with_config(&cfg.logging)?;

    // Install status provider so IPC/status can respond.
    init_basic_status_provider();

    if cfg.metrics.enabled {
        let metrics = Arc::new(init_metrics_from_config(&cfg.metrics)?);
        set_global_metrics(metrics);
    }

    // Background scheduler sampling loop; real queues/workers will hook in later.
    let sched_cfg = SchedulerConfig {
        warm_idle: Duration::from_secs(cfg.scheduler.idle_warm_seconds),
        deep_idle: Duration::from_secs(cfg.scheduler.idle_deep_seconds),
        cpu_metadata_max: cfg.scheduler.cpu_soft_limit_pct as f32,
        cpu_content_max: cfg.scheduler.cpu_hard_limit_pct as f32,
        disk_busy_threshold_bps: cfg.scheduler.disk_busy_bytes_per_s,
        content_batch_size: cfg.scheduler.content_batch_size as usize,
        ..SchedulerConfig::default()
    };
    let sample_every = Duration::from_secs(cfg.metrics.sample_interval_secs.max(1));
    thread::spawn(move || {
        let mut runtime = SchedulerRuntime::new(sched_cfg);
        loop {
            let _ = runtime.tick();
            thread::sleep(sample_every);
        }
    });

    // Try to install metadata search handler (optional; fallback is stub).
    if let Ok(handler) = MetaIndexSearchHandler::try_new(Path::new(&cfg.paths.meta_index)) {
        set_search_handler(Box::new(handler));
    } else {
        tracing::warn!("meta-index search handler not initialized; falling back to stub");
    }

    println!("UltraSearch service placeholder – scheduler sampling active.");

    Ok(())
}
