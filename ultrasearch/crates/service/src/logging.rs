use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use core_types::config::LoggingSection;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize tracing/logging for the service process using the provided config.
///
/// - Honors `logging.level` from config, falling back to `RUST_LOG` then `info`.
/// - Writes JSON logs to the configured rolling file (daily) and stdout (json or text per cfg).
pub fn init_tracing_with_config(
    cfg: &LoggingSection,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    // Determine filter: explicit level in config, else RUST_LOG, else info.
    let filter_str = if !cfg.level.is_empty() {
        cfg.level.clone()
    } else {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
    };
    let filter = EnvFilter::new(filter_str);

    // Split logging file into directory + filename.
    let log_path = PathBuf::from(&cfg.file);
    let (dir, file) = split_dir_file(&log_path)?;
    if !dir.exists() {
        fs::create_dir_all(dir).context("create log directory")?;
    }

    let file_appender = match cfg.roll.as_str() {
        "hourly" => tracing_appender::rolling::hourly(dir, file),
        "daily" => tracing_appender::rolling::daily(dir, file),
        "minutely" => tracing_appender::rolling::minutely(dir, file),
        other => {
            // "size" or unknown fallback to daily for now.
            // TODO: Implement size-based rotation and cleanup (retain).
            tracing::warn!("Log rotation '{}' not fully supported; falling back to daily.", other);
            tracing_appender::rolling::daily(dir, file)
        }
    };

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // File layer always JSON.
    let file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true);

    // Try to init. If it fails (already set), we just return the guard?
    // Wait, if we don't init, the guard might be useless if the subscriber isn't using it.
    // But if it's already set, we can't change it.
    // We'll log a warning if we can't init.
    
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer);

    let result = if cfg.format.as_str() == "json" {
        registry
            .with(
                fmt::layer()
                    .json()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_line_number(true),
            )
            .try_init()
    } else {
        registry
            .with(
                fmt::layer()
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_line_number(true),
            )
            .try_init()
    };

    if let Err(e) = result {
        eprintln!("Tracing init failed (global subscriber already set?): {}", e);
    }

    Ok(guard)
}

/// Backward-compatible initializer using defaults.
pub fn init_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let default = LoggingSection::default();
    init_tracing_with_config(&default)
}

fn split_dir_file(path: &Path) -> Result<(&Path, &str)> {
    let dir = path.parent().context("log file missing parent directory")?;
    let file = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("log file missing filename")?;
    Ok((dir, file))
}
