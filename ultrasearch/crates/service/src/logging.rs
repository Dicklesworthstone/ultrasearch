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

    // Size-based rotation with a conservative default (2 MiB). We avoid deleting old logs to
    // honor the no-delete policy; rotated files are timestamp-suffixed.
    let max_bytes = cfg.max_size_mb.saturating_mul(1024 * 1024);
    let sized_writer = SizeRotatingWriter::new(dir.to_path_buf(), file.to_string(), max_bytes)?;

    let (non_blocking, guard) = tracing_appender::non_blocking(sized_writer);

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

/// A simple size-rotating writer: when the active log exceeds `max_bytes`,
/// the file is renamed with a timestamp suffix and a new file is opened.
#[derive(Clone)]
struct SizeRotatingWriter {
    inner: Arc<Inner>,
}

struct Inner {
    dir: PathBuf,
    file_name: String,
    max_bytes: u64,
    state: Mutex<WriterState>,
}

struct WriterState {
    file: File,
    current_size: u64,
}

impl SizeRotatingWriter {
    fn new(dir: PathBuf, file_name: String, max_bytes: u64) -> io::Result<Self> {
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
        }
        let path = dir.join(&file_name);
        let file = File::options().create(true).append(true).open(&path)?;
        let current_size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Inner {
                dir,
                file_name,
                max_bytes: max_bytes.max(1), // guard against zero
                state: Mutex::new(WriterState { file, current_size }),
            }),
        })
    }

    fn rotate_locked(inner: &Inner, state: &mut WriterState) -> io::Result<()> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let rotated_name = format!("{}.{}", inner.file_name, timestamp);
        let rotated_path = inner.dir.join(rotated_name);
        let active_path = inner.dir.join(&inner.file_name);
        // Close current file by dropping it after rename.
        drop(std::mem::replace(
            &mut state.file,
            File::options()
                .create(true)
                .write(true)
                .truncate(true)
                .open(inner.dir.join("__temp__.log"))?,
        ));
        let _ = fs::rename(&active_path, &rotated_path);
        let new_file = File::options()
            .create(true)
            .append(true)
            .open(&active_path)?;
        *state = WriterState {
            file: new_file,
            current_size: 0,
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
        if state.current_size + buf.len() as u64 > self.inner.max_bytes {
            SizeRotatingWriter::rotate_locked(&self.inner, &mut state)?;
        }
        let written = state.file.write(buf)?;
        state.current_size = state
            .current_size
            .saturating_add(written as u64)
            .min(self.inner.max_bytes);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.inner.state.lock();
        state.file.flush()
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
