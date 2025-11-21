use std::{
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use core_types::config::AppConfig;
use ipc::VolumeStatus;
use ntfs_watcher::{NtfsError, discover_volumes, enumerate_mft};
use scheduler::SchedulerConfig;
use tokio::sync::mpsc;

use crate::{
    init_tracing_with_config,
    meta_ingest::ingest_with_paths,
    metrics::{init_metrics_from_config, set_global_metrics},
    scheduler_runtime::SchedulerRuntime,
    search_handler::{MetaIndexSearchHandler, set_search_handler},
    status_provider::{
        init_basic_status_provider, update_status_last_commit, update_status_volumes,
    },
};

pub fn run_app(cfg: &AppConfig, mut shutdown_rx: mpsc::Receiver<()>) -> Result<()> {
    let _guard = init_tracing_with_config(&cfg.logging)?;

    // Install status provider so IPC/status can respond.
    init_basic_status_provider();

    if cfg.metrics.enabled {
        let metrics = Arc::new(init_metrics_from_config(&cfg.metrics)?);
        set_global_metrics(metrics);
    }

    run_initial_metadata_ingest(cfg)?;

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
    
    // We'll use a dedicated thread for the scheduler loop for now,
    // but in a real service, we might want to use the tokio runtime.
    // For now, we just spawn it.
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

    tracing::info!("UltraSearch service started. Waiting for shutdown signal...");

    // Block until shutdown signal
    // In a real async app, we would await; here we blocking_recv since we are not in an async fn (yet).
    // But `run_app` is called from `main` (sync) or `service_main` (sync).
    // However, `shutdown_rx` is async. We need a runtime if we want to await, or use blocking_recv.
    // Since the channel is mpsc, we can use `blocking_recv`.
    
    let _ = shutdown_rx.blocking_recv();
    
    tracing::info!("Shutdown signal received. Exiting.");
    Ok(())
}

fn run_initial_metadata_ingest(cfg: &AppConfig) -> Result<()> {
    let volumes = match discover_volumes() {
        Ok(v) if v.is_empty() => {
            tracing::info!("no NTFS volumes discovered; skipping initial metadata ingest");
            return Ok(());
        }
        Ok(v) => v,
        Err(NtfsError::NotSupported) => {
            tracing::info!("platform does not support NTFS watcher; skipping metadata ingest");
            return Ok(());
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to discover volumes; skipping metadata ingest");
            return Ok(());
        }
    };

    let mut status = Vec::with_capacity(volumes.len());

    for volume in volumes {
        tracing::info!(guid = %volume.guid_path, letters = ?volume.drive_letters, "enumerating MFT for volume");
        match enumerate_mft(&volume) {
            Ok(metas) => {
                if metas.is_empty() {
                    tracing::info!(guid = %volume.guid_path, "no entries found during MFT enumeration");
                    continue;
                }

                let count = metas.len() as u64;
                tracing::info!(guid = %volume.guid_path, files = count, "ingesting metadata batch into meta-index");
                ingest_with_paths(&cfg.paths, metas, None)?;

                status.push(VolumeStatus {
                    volume: volume.id,
                    indexed_files: count,
                    pending_files: 0,
                    last_usn: None,
                    journal_id: None,
                });

                update_status_last_commit(Some(unix_timestamp_secs()));
            }
            Err(err) => {
                tracing::warn!(guid = %volume.guid_path, error = %err, "failed to enumerate MFT; skipping volume");
            }
        }
    }

    if !status.is_empty() {
        update_status_volumes(status);
    }

    Ok(())
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
