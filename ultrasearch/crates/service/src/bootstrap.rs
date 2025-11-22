use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use core_types::config::AppConfig;
use ipc::VolumeStatus;
use ntfs_watcher::{NtfsError, discover_volumes, enumerate_mft};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct BootstrapOptions {
    /// If provided, seed the meta index with these file entries instead of discovering NTFS volumes.
    pub initial_metas: Option<Vec<core_types::FileMeta>>,
    /// Skip initial ingest entirely (used for tests that want a blank service).
    pub skip_initial_ingest: bool,
    /// Override IPC pipe name (default is \\\\.\\pipe\\ultrasearch).
    pub pipe_name: Option<String>,
}

use crate::{
    init_tracing_with_config,
    meta_ingest::ingest_with_paths,
    metrics::{init_metrics_from_config, set_global_metrics},
    scheduler_runtime::SchedulerRuntime,
    search_handler::set_search_handler,
    status_provider::{
        init_basic_status_provider, update_status_last_commit, update_status_volumes,
    },
};

pub fn run_app(cfg: &AppConfig, shutdown_rx: mpsc::Receiver<()>) -> Result<()> {
    run_app_with_options(cfg, shutdown_rx, BootstrapOptions::default())
}

pub fn run_app_with_options(
    cfg: &AppConfig,
    mut shutdown_rx: mpsc::Receiver<()>,
    opts: BootstrapOptions,
) -> Result<()> {
    let _guard = init_tracing_with_config(&cfg.logging)?;

    // Initialize Tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _rt_guard = rt.enter();

    // Install status provider so IPC/status can respond.
    init_basic_status_provider();

    if cfg.metrics.enabled {
        let metrics = Arc::new(init_metrics_from_config(&cfg.metrics)?);
        set_global_metrics(metrics);
    }

    match opts.initial_metas {
        Some(metas) => ingest_seed_metadata(cfg, metas)?,
        None if opts.skip_initial_ingest => {
            tracing::info!("skip_initial_ingest=true; leaving indices empty");
        }
        None => run_initial_metadata_ingest(cfg)?,
    }

    // Start scheduler loop
    // We need to clone cfg for the scheduler (or pass reference if new() takes ref).
    // SchedulerRuntime::new takes &AppConfig.
    let scheduler = SchedulerRuntime::new(cfg);
    rt.spawn(scheduler.run_loop());

    // Try to install unified search handler.
    // We pass both meta and content index paths.
    let meta_path = Path::new(&cfg.paths.meta_index);
    let content_path = Path::new(&cfg.paths.content_index);

    let mut attempts = 0;
    loop {
        match crate::search_handler::UnifiedSearchHandler::try_new(meta_path, content_path) {
            Ok(handler) => {
                set_search_handler(Box::new(handler));
                break;
            }
            Err(e) => {
                // Check if error string contains "corruption" or "corrupted" or similar tantivy errors.
                // Tantivy errors are opaque via anyhow, so string check is a heuristic.
                let msg = e.to_string().to_lowercase();
                let is_corruption =
                    msg.contains("corrupt") || msg.contains("format") || msg.contains("lock");

                if is_corruption && attempts < 1 {
                    tracing::warn!(
                        "Index corruption detected ({}), attempting recovery...",
                        msg
                    );
                    // Rename broken index if it exists
                    if meta_path.exists() {
                        let broken = meta_path.with_extension("broken");
                        let _ = std::fs::rename(meta_path, &broken);
                        tracing::info!("Renamed corrupt meta index to {:?}", broken);
                    }
                    // Content index might be fine, but let's be safe and rename it too if opening failed generally?
                    // UnifiedSearchHandler tries both. If meta fails, we fail.
                    // If content fails, we log warning but return handler (in try_new implementation).
                    // So if try_new returns Err, it's likely meta-index issue or critical content issue.
                    // Let's wipe both if we can't determine source easily, or just meta.
                    // For simplicity in this resilience task, we wipe meta.
                    attempts += 1;
                    continue;
                }

                tracing::warn!("unified search handler not initialized: {}", e);
                break;
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Start IPC server
        // We use the runtime we just created.
        if let Err(e) = rt.block_on(crate::ipc::start_pipe_server(opts.pipe_name.as_deref())) {
            tracing::error!("failed to start IPC server: {}", e);
        }
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

fn ingest_seed_metadata(cfg: &AppConfig, metas: Vec<core_types::FileMeta>) -> Result<()> {
    if metas.is_empty() {
        tracing::info!("seed metadata list empty; skipping ingest");
        return Ok(());
    }

    ingest_with_paths(&cfg.paths, metas.clone(), None)?;

    let mut by_vol: std::collections::HashMap<core_types::VolumeId, u64> =
        std::collections::HashMap::new();
    for meta in &metas {
        *by_vol.entry(meta.volume).or_insert(0) += 1;
    }

    let mut status = Vec::with_capacity(by_vol.len());
    for (vol, count) in by_vol {
        status.push(VolumeStatus {
            volume: vol,
            indexed_files: count,
            pending_files: 0,
            last_usn: None,
            journal_id: None,
        });
    }

    update_status_last_commit(Some(unix_timestamp_secs()));
    update_status_volumes(status);
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
