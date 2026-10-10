use std::{
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use core_types::config::AppConfig;
use ipc::VolumeStatus;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Default)]
pub struct BootstrapOptions {
    /// If provided, seed the meta index with these file entries instead of discovering NTFS volumes.
    pub initial_metas: Option<Vec<core_types::FileMeta>>,
    /// Skip initial ingest entirely (used for tests that want a blank service).
    pub skip_initial_ingest: bool,
    /// Override IPC pipe name (default is \\\\.\\pipe\\ultrasearch).
    pub pipe_name: Option<String>,
    /// Force scheduler to run content jobs even if idle/load gates are active (tests).
    pub force_content_jobs: bool,
}

use crate::{
    init_tracing_with_config,
    meta_ingest::ingest_with_paths,
    metrics::{init_metrics_from_config, set_global_metrics},
    priority::apply_background_priorities,
    scanner::{initialize_indexes, watch_changes},
    scheduler_runtime::SchedulerRuntime,
    search_handler::set_search_handler,
    status_provider::{
        init_basic_status_provider, update_status_ingestion_state, update_status_last_commit,
        update_status_volumes,
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
    // Always drop to background-friendly priorities before heavy work.
    apply_background_priorities();

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

    let mut pending_jobs = Vec::new();

    let mut cfg_owned = cfg.clone();
    super::ensure_default_volumes(&mut cfg_owned)?;
    ensure_data_paths_exist(&cfg_owned)?;
    core_types::config::set_current_config(cfg_owned.clone())?;
    let ingestion = initialize_indexes(&cfg_owned)?;

    let use_journal = opts.initial_metas.is_none() && !opts.skip_initial_ingest;
    match opts.initial_metas {
        Some(metas) => ingest_seed_metadata(&cfg_owned, metas, &mut pending_jobs)?,
        None if opts.skip_initial_ingest => {
            tracing::info!("skip_initial_ingest=true; leaving indices empty");
        }
        None => tracing::info!(
            "MFT reconciliation and journal replay will run on the durable ingestion lane"
        ),
    }

    // Start scheduler loop
    // We need to clone cfg for the scheduler (or pass reference if new() takes ref).
    // SchedulerRuntime::new takes &AppConfig.
    let mut scheduler = SchedulerRuntime::new(&cfg_owned);
    if opts.force_content_jobs {
        scheduler.force_allow_content();
    }
    if !pending_jobs.is_empty() {
        tracing::info!(
            "Seeding {} content jobs into scheduler queue",
            pending_jobs.len()
        );
        scheduler
            .submit_content_jobs(pending_jobs)
            .map_err(|rejected| {
                anyhow::anyhow!(
                    "seed content queue limit exceeded; {} jobs were not admitted",
                    rejected.len()
                )
            })?;
    }
    rt.spawn(scheduler.run_loop());

    // Start after the scheduler exists. Test seeds explicitly opt out of native
    // discovery; production never substitutes a successful no-op watcher.
    if use_journal {
        let cfg_clone = cfg_owned.clone();
        rt.spawn(async move {
            if let Err(e) = watch_changes(cfg_clone, ingestion).await {
                update_status_ingestion_state(format!("stopped: {e:#}"));
                tracing::error!("change watcher stopped: {e:#}");
            }
        });
    } else {
        update_status_ingestion_state("native ingestion disabled by test bootstrap options");
    }

    // Try to install unified search handler.
    // We pass both meta and content index paths.
    let meta_path = Path::new(&cfg_owned.paths.meta_index);
    let content_path = Path::new(&cfg_owned.paths.content_index);

    // Schema migration was completed before starting writers. Do not rename an
    // index out from under an active worker based on an error-string heuristic.
    let handler = crate::search_handler::UnifiedSearchHandler::try_new(meta_path, content_path)?;
    set_search_handler(Box::new(handler));

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

/// Make sure all configured data paths exist so worker processes don’t fail with ENOENT.
fn ensure_data_paths_exist(cfg: &AppConfig) -> Result<()> {
    use std::fs;
    let paths = [
        &cfg.paths.meta_index,
        &cfg.paths.content_index,
        &cfg.paths.state_dir,
        &cfg.paths.jobs_dir,
    ];

    for p in paths {
        let path = std::path::Path::new(p);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // If path itself is meant to be a directory (indexes/dirs), create it too.
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn ingest_seed_metadata(
    cfg: &AppConfig,
    metas: Vec<core_types::FileMeta>,
    pending_jobs: &mut Vec<crate::dispatcher::job_dispatch::JobSpec>,
) -> Result<()> {
    if metas.is_empty() {
        tracing::info!("seed metadata list empty; skipping ingest");
        return Ok(());
    }

    ingest_with_paths(&cfg.paths, metas.clone(), None)?;

    let mut by_vol: std::collections::HashMap<core_types::VolumeId, (u64, u64)> =
        std::collections::HashMap::new();
    for meta in &metas {
        let entry = by_vol.entry(meta.volume).or_insert((0, 0));
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(meta.size);
    }

    let mut status = Vec::with_capacity(by_vol.len());
    for (vol, (count, bytes)) in by_vol {
        status.push(VolumeStatus {
            volume: vol,
            indexed_files: count,
            indexed_bytes: bytes,
            pending_files: 0,
            pending_bytes: 0,
            last_usn: None,
            journal_id: None,
        });
    }

    // Seed content jobs for any provided seed files (if they have paths).
    for meta in metas {
        if let Some(job) = crate::scheduler_runtime::content_job_from_meta(&meta, &cfg.extract) {
            pending_jobs.push(job);
        }
    }

    update_status_last_commit(Some(unix_timestamp_secs()));
    update_status_volumes(status);
    Ok(())
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
