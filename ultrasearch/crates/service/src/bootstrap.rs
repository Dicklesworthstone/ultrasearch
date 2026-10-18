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
    priority::apply_service_priorities,
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
    let _guard = init_tracing_with_config(&cfg.logging)?;

    // The resident service answers interactive queries and advances journals.
    // Only extraction workers enter idle/background scheduling. Apply before
    // spawning runtime threads, and report OS failures instead of hiding them.
    apply_service_priorities()?;

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

    // Empty scopes mean automatic discovery on every watcher turn, not a
    // one-time snapshot of mounted drive letters. Startup must not freeze that
    // policy or serialize this expanded (possibly custom-source) configuration
    // over the unrelated default configuration file.
    let cfg_owned = cfg.clone();
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
    // Try to install unified search handler.
    // We pass both meta and content index paths.
    let meta_path = Path::new(&cfg_owned.paths.meta_index);
    let content_path = Path::new(&cfg_owned.paths.content_index);

    // Schema migration was completed before starting writers. Do not rename an
    // index out from under an active worker based on an error-string heuristic.
    let handler = crate::search_handler::UnifiedSearchHandler::try_new(meta_path, content_path)?;
    set_search_handler(Box::new(handler));

    #[cfg(target_os = "windows")]
    let _pipe_listener = rt.block_on(crate::ipc::start_pipe_server(opts.pipe_name.as_deref()))?;

    // Readers and the endpoint are ready before writers can run. A failed pipe
    // bind returns an error, drops this ingestion session, and never leaves a
    // second invisible scanner/worker running behind another service's pipe.
    rt.spawn(scheduler.run_loop());
    // Test seeds explicitly opt out of native discovery; production never
    // substitutes a successful no-op watcher.
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, ensure};
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn startup_preserves_automatic_scope_and_the_custom_configuration_source() -> Result<()> {
        const CHILD_ROOT: &str = "ULTRASEARCH_BOOTSTRAP_CONFIG_TEST_ROOT";
        const PROOF: &[u8] = b"automatic-scope-and-custom-source-preserved";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let source = root.join("custom.toml");
            let default_path = core_types::config::default_config_path();
            let source_before = fs::read(&source)?;
            let default_before = fs::read(&default_path)?;
            let cfg = core_types::config::load_or_create_config(Some(&source))?;
            ensure!(cfg.volumes.is_empty() && cfg.content_index_volumes.is_empty());
            let (shutdown, receiver) = mpsc::channel(1);
            shutdown.try_send(())?;
            // Exercise real bootstrap, readers, state and (on Windows) the
            // named pipe without mutating or scanning the host's filesystems.
            // Native discovery belongs exclusively to watch_changes, not this
            // explicit disabled-ingestion bootstrap.
            run_app_with_options(
                &cfg,
                receiver,
                BootstrapOptions {
                    skip_initial_ingest: true,
                    pipe_name: Some(format!(r"\\.\pipe\ultrasearch-bootstrap-{}", uuid::Uuid::new_v4())),
                    ..BootstrapOptions::default()
                },
            )?;
            let active = core_types::config::get_current_config();
            ensure!(active.volumes.is_empty(), "startup froze automatic metadata scope");
            ensure!(active.content_index_volumes.is_empty(), "startup froze automatic content scope");
            ensure!(fs::read(&source)? == source_before, "startup rewrote its custom source");
            ensure!(fs::read(&default_path)? == default_before, "startup overwrote the default configuration");
            // The default file is deliberately invalid. Reload(None) must use
            // the custom source retained by the production configuration API.
            let mut next = cfg.clone();
            next.extract.max_chars_per_file += 1;
            fs::write(&source, toml::to_string_pretty(&next)?)?;
            let reloaded = core_types::config::reload_config(None)?;
            ensure!(reloaded.extract.max_chars_per_file == next.extract.max_chars_per_file);
            ensure!(reloaded.volumes.is_empty() && reloaded.content_index_volumes.is_empty());
            fs::write(root.join("proof.txt"), PROOF)?;
            return Ok(());
        }

        // Isolate tracing, process priorities, global readers/configuration and
        // PROGRAMDATA from the parent running the rest of the test suite.
        let root = tempfile::tempdir()?.keep();
        let program_data = root.join("system-data");
        let default_path = program_data.join("UltraSearch/config/config.toml");
        fs::create_dir_all(default_path.parent().context("default config parent missing")?)?;
        fs::write(&default_path, b"unrelated configuration: preserve these exact bytes")?;
        let mut cfg = AppConfig::default();
        cfg.app.data_dir = root.join("test-data").to_str().context("fixture path is not UTF-8")?.into();
        fs::write(root.join("custom.toml"), toml::to_string_pretty(&cfg)?)?;
        let name = concat!(module_path!(), "::startup_preserves_automatic_scope_and_the_custom_configuration_source");
        let name = name.split_once("::").context("test crate prefix missing")?.1;
        let mut child = ChildGuard(
            Command::new(std::env::current_exe()?)
                .args(["--exact", name, "--nocapture", "--test-threads=1"])
                .env(CHILD_ROOT, &root)
                .env("PROGRAMDATA", &program_data)
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(status.success(), "bootstrap regression child failed: {status}");
                ensure!(fs::read(root.join("proof.txt"))? == PROOF, "bootstrap assertions did not execute");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "bootstrap regression child timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
