use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use core_types::config::LoggingSection;
use parking_lot::Mutex;
use std::sync::OnceLock;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, MakeWriter},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

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

    // Size- and period-based rotation with bounded retention ring (`retain` slots).
    let max_bytes = cfg.max_size_mb.saturating_mul(1024 * 1024);
    let sized_writer = SizeRotatingWriter::with_options(
        dir.to_path_buf(),
        file.to_string(),
        max_bytes,
        cfg.retain,
        &cfg.roll,
    )?;

    let (non_blocking, guard) = tracing_appender::non_blocking(sized_writer);

    // File layer always JSON.
    let file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true);

    let registry = tracing_subscriber::registry().with(filter).with(file_layer);

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
        static WARNED_ONCE: OnceLock<()> = OnceLock::new();
        // Common in tests when multiple runtimes initialize tracing.
        let msg = e.to_string();
        if !msg.contains("already set") && WARNED_ONCE.set(()).is_ok() {
            eprintln!(
                "Tracing init failed (global subscriber already set?): {}",
                msg
            );
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollPolicy {
    Daily,
    Hourly,
    SizeOnly,
}

impl RollPolicy {
    fn from_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hourly" => Self::Hourly,
            "size" => Self::SizeOnly,
            _ => Self::Daily,
        }
    }

    fn current_bucket(self) -> Option<u64> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self {
            Self::Daily => Some(secs / 86_400),
            Self::Hourly => Some(secs / 3_600),
            Self::SizeOnly => None,
        }
    }
}

/// A size- and period-rotating writer with a bounded retention ring (`retain` files).
#[derive(Clone)]
struct SizeRotatingWriter {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    file_name: String,
    max_bytes: u64,
    retain: u32,
    roll_policy: RollPolicy,
    state: Mutex<WriterState>,
}

struct WriterState {
    file: Option<File>,
    current_size: u64,
    period_bucket: Option<u64>,
}

impl SizeRotatingWriter {
    #[allow(dead_code)]
    fn new(dir: PathBuf, file_name: String, max_bytes: u64) -> io::Result<Self> {
        Self::with_options(dir, file_name, max_bytes, 7, "daily")
    }

    fn with_options(
        dir: PathBuf,
        file_name: String,
        max_bytes: u64,
        retain: u32,
        roll: &str,
    ) -> io::Result<Self> {
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let roll_policy = RollPolicy::from_str(roll);
        let path = dir.join(&file_name);
        let file = File::options().create(true).append(true).open(&path)?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                file_name,
                max_bytes: max_bytes.max(1),
                retain: retain.max(1),
                roll_policy,
                state: Mutex::new(WriterState {
                    file: Some(file),
                    current_size,
                    period_bucket: roll_policy.current_bucket(),
                }),
            }),
        })
    }

    fn rotate_locked(inner: &Inner, state: &mut WriterState) -> io::Result<()> {
        let active_path = inner.dir.join(&inner.file_name);
        // Drop active file handle before renaming so Windows allows moving the file.
        drop(state.file.take());

        // Shift bounded retention slots: `.{retain-1}` -> `.{retain}`, ..., `.1` -> `.2`
        for idx in (1..inner.retain).rev() {
            let src = inner.dir.join(format!("{}.{}", inner.file_name, idx));
            let dst = inner.dir.join(format!("{}.{}", inner.file_name, idx + 1));
            if src.exists() {
                let _ = fs::rename(&src, &dst);
            }
        }
        let first_slot = inner.dir.join(format!("{}.1", inner.file_name));
        let _ = fs::rename(&active_path, &first_slot);

        let new_file = File::options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&active_path)?;
        *state = WriterState {
            file: Some(new_file),
            current_size: 0,
            period_bucket: inner.roll_policy.current_bucket(),
        };
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for SizeRotatingWriter {
    type Writer = SizeRotatingWriterHandle;

    fn make_writer(&'a self) -> Self::Writer {
        SizeRotatingWriterHandle {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Clone)]
struct SizeRotatingWriterHandle {
    inner: Arc<Inner>,
}

impl Write for SizeRotatingWriterHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self.inner.state.lock();
        let now_bucket = self.inner.roll_policy.current_bucket();
        let period_changed = now_bucket != state.period_bucket && state.current_size > 0;
        let size_exceeded = state.current_size + buf.len() as u64 > self.inner.max_bytes;
        if period_changed || size_exceeded {
            SizeRotatingWriter::rotate_locked(&self.inner, &mut state)?;
        }
        let file = match state.file.as_mut() {
            Some(f) => f,
            None => {
                let active_path = self.inner.dir.join(&self.inner.file_name);
                let reopened = File::options()
                    .create(true)
                    .append(true)
                    .open(&active_path)?;
                state.file = Some(reopened);
                state.file.as_mut().expect("just inserted")
            }
        };
        let written = file.write(buf)?;
        state.current_size = state
            .current_size
            .saturating_add(written as u64)
            .min(self.inner.max_bytes);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.inner.state.lock();
        if let Some(file) = state.file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }
}

// Allow passing the writer directly into tracing_appender::non_blocking.
impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut handle = self.make_writer();
        handle.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut handle = self.make_writer();
        handle.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn size_rotating_writer_caps_retained_files_without_temp_leak() -> io::Result<()> {
        let dir = tempdir()?;
        let mut writer = SizeRotatingWriter::with_options(
            dir.path().to_path_buf(),
            "searchd.log".into(),
            16,
            2,
            "size",
        )?;

        for i in 0..5 {
            writeln!(writer, "line-number-{i:04}")?;
        }
        writer.flush()?;

        assert!(dir.path().join("searchd.log").exists());
        assert!(dir.path().join("searchd.log.1").exists());
        assert!(dir.path().join("searchd.log.2").exists());
        assert!(!dir.path().join("searchd.log.3").exists());
        assert!(!dir.path().join("__temp__.log").exists());
        Ok(())
    }
}
