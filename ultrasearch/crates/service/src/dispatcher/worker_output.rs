//! Bounded worker output transport and last-problem evidence for service hosts.
//!
//! The Windows service may have no usable inherited stdout/stderr. Drain both
//! pipes concurrently, even after the retained tail fills. Diagnostics are not
//! commit receipts and must never determine journal acknowledgement or health.

use super::job_dispatch::{IndexBatch, JobOperation};
use anyhow::{Context, Result, ensure};
use core_types::DocKey;
use serde::Serialize;
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const TAIL_BYTES: usize = 32 * 1024;
const REPORT_JOBS: usize = 128;
const REPORT_BYTES: usize = 1024 * 1024;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const REPORT_NAME: &str = "worker-diagnostics.json";
const STAGING_NAME: &str = "worker-diagnostics.next.json";
static READERS: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub(super) fn configure_command(command: &mut Command) {
    command
        // Content previews are interactive output, not service diagnostics.
        // Keep the worker's existing CLI/receipt contract; do not request JSON
        // extraction output, which would copy document bodies into the log.
        .arg("--preview-chars")
        .arg("0")
        // Warnings must survive an absent/disabled inherited tracing filter.
        .env("RUST_LOG", "warn")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
}

/// Reserve before spawning. An inherited pipe held open after worker exit can
/// leave a reader blocked. Such a reader retains its slot across scheduler
/// replacement; retries cannot accumulate unbounded detached reader threads.
pub(super) struct OutputReservation {
    stdout: OwnedSemaphorePermit,
    stderr: OwnedSemaphorePermit,
}

impl OutputReservation {
    pub fn reserve() -> Result<Self> {
        Self::from_pool(Arc::clone(READERS.get_or_init(|| Arc::new(Semaphore::new(2)))))
    }

    fn from_pool(pool: Arc<Semaphore>) -> Result<Self> {
        let stdout = Arc::clone(&pool)
            .try_acquire_owned()
            .context("previous worker output is still draining; retry before spawning")?;
        let stderr = pool
            .try_acquire_owned()
            .context("previous worker output is still draining; retry before spawning")?;
        Ok(Self { stdout, stderr })
    }

    pub fn start(self, child: &mut Child) -> Result<WorkerOutput> {
        let stdout = child.stdout.take().context("worker stdout pipe missing")?;
        let stderr = child.stderr.take().context("worker stderr pipe missing")?;
        self.start_readers(stdout, stderr)
    }

    fn start_readers(
        self,
        stdout: impl Read + Send + 'static,
        stderr: impl Read + Send + 'static,
    ) -> Result<WorkerOutput> {
        let (done_tx, done_rx) = mpsc::channel();
        let stdout = start_reader(stdout, self.stdout, done_tx.clone(), "stdout")?;
        let stderr = start_reader(stderr, self.stderr, done_tx, "stderr")?;
        Ok(WorkerOutput {
            stdout,
            stderr,
            done_rx,
        })
    }
}

#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    total_bytes: u64,
    complete: bool,
    error: Option<String>,
}

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);
        if bytes.len() >= TAIL_BYTES {
            self.bytes.clear();
            self.bytes.extend(&bytes[bytes.len() - TAIL_BYTES..]);
        } else {
            let remove = (self.bytes.len() + bytes.len()).saturating_sub(TAIL_BYTES);
            self.bytes.drain(..remove);
            self.bytes.extend(bytes);
        }
    }

    fn snapshot(&self) -> StreamOutput {
        let bytes: Vec<_> = self.bytes.iter().copied().collect();
        StreamOutput {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            total_bytes: self.total_bytes,
            dropped_bytes: self.total_bytes.saturating_sub(bytes.len() as u64),
            complete: self.complete,
            error: self.error.clone(),
        }
    }
}

fn start_reader(
    mut reader: impl Read + Send + 'static,
    slot: OwnedSemaphorePermit,
    done: mpsc::Sender<()>,
    name: &str,
) -> Result<Arc<Mutex<Tail>>> {
    let tail = Arc::new(Mutex::new(Tail::default()));
    let output = Arc::clone(&tail);
    std::thread::Builder::new()
        .name(format!("index-worker-{name}"))
        .spawn(move || {
            let mut bytes = [0u8; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => {
                        output.lock().unwrap_or_else(|e| e.into_inner()).complete = true;
                        break;
                    }
                    Ok(length) => {
                        output
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(&bytes[..length]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        output.lock().unwrap_or_else(|e| e.into_inner()).error =
                            Some(bounded_text(&error.to_string(), 512).0);
                        break;
                    }
                }
            }
            // Completion notification establishes closed transport ownership,
            // not merely EOF observed before dropping the pipe and its slot.
            drop(reader);
            drop(slot);
            let _ = done.send(());
        })
        .with_context(|| format!("cannot start worker {name} drain"))?;
    Ok(tail)
}

pub(super) struct WorkerOutput {
    stdout: Arc<Mutex<Tail>>,
    stderr: Arc<Mutex<Tail>>,
    done_rx: mpsc::Receiver<()>,
}

#[derive(Debug, Serialize)]
pub(super) struct CapturedOutput {
    stdout: StreamOutput,
    stderr: StreamOutput,
}

#[derive(Debug, Serialize)]
struct StreamOutput {
    text: String,
    total_bytes: u64,
    dropped_bytes: u64,
    complete: bool,
    error: Option<String>,
}

impl WorkerOutput {
    pub fn finish(self) -> CapturedOutput {
        self.finish_with_timeout(DRAIN_TIMEOUT)
    }

    fn finish_with_timeout(self, timeout: Duration) -> CapturedOutput {
        let started = Instant::now();
        for _ in 0..2 {
            if self
                .done_rx
                .recv_timeout(timeout.saturating_sub(started.elapsed()))
                .is_err()
            {
                break;
            }
        }
        CapturedOutput {
            stdout: self.stdout.lock().unwrap_or_else(|e| e.into_inner()).snapshot(),
            stderr: self.stderr.lock().unwrap_or_else(|e| e.into_inner()).snapshot(),
        }
    }
}

impl CapturedOutput {
    pub fn incomplete(&self) -> bool {
        !self.stdout.complete || !self.stderr.complete
    }

    pub fn log_warnings(&self, batch: uuid::Uuid) {
        // With zero preview characters, normal output is only blank lines.
        // Preserve permanent-omission warnings even on a nondeferred success,
        // but do not replace the last failed/deferred attempt's disk evidence.
        if !self.stdout.text.trim().is_empty() || !self.stderr.text.trim().is_empty() {
            tracing::warn!(%batch, stdout = ?self.stdout, stderr = ?self.stderr,
                "worker completed with diagnostic output");
        }
    }
}

#[derive(Serialize)]
struct JobEvidence {
    key: String,
    volume: u16,
    file_reference: String,
    operation: JobOperation,
    admitted_path: String,
    path_truncated: bool,
}

#[derive(Serialize)]
struct ProblemReport<'a> {
    version: u32,
    batch_id: uuid::Uuid,
    observed_unix_seconds: u64,
    outcome: String,
    // None means no successful matching receipt: these are candidate jobs,
    // not a fabricated list of committed per-file deferrals.
    deferred_count: Option<usize>,
    jobs: Vec<JobEvidence>,
    omitted_jobs: usize,
    output: &'a CapturedOutput,
}

fn bounded_text(value: &str, limit: usize) -> (String, bool) {
    let mut chars = value.chars();
    let text: String = chars.by_ref().take(limit).collect();
    (text, chars.next().is_some())
}

/// Last problematic attempt, not a live retry ledger or an acknowledgement.
/// The caller still owns the worker lane while replacing this evidence. Keep
/// one final and one staging file instead of creating a file for every retry.
pub(super) fn record_problem(
    jobs_dir: &Path,
    batch: &IndexBatch,
    outcome: &str,
    deferred: Option<&[DocKey]>,
    output: &CapturedOutput,
) -> Result<PathBuf> {
    let mut jobs = Vec::new();
    let mut matching_jobs = 0usize;
    for job in &batch.jobs {
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        if deferred.is_some_and(|keys| !keys.contains(&key)) {
            continue;
        }
        matching_jobs += 1;
        if jobs.len() < REPORT_JOBS {
            let (path, truncated) = bounded_text(&job.path.to_string_lossy(), 512);
            jobs.push(JobEvidence {
                key: key.to_string(),
                volume: job.volume_id,
                file_reference: format!("0x{:016x}", job.file_id),
                operation: job.operation,
                admitted_path: path,
                path_truncated: truncated,
            });
        }
    }
    let report = ProblemReport {
        version: 1,
        batch_id: batch.id,
        observed_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        outcome: bounded_text(outcome, 2048).0,
        deferred_count: deferred.map(<[DocKey]>::len),
        omitted_jobs: matching_jobs.saturating_sub(jobs.len()),
        jobs,
        output,
    };
    // The service log gets bounded diagnostic fields as well. Debug-format
    // paths and captured text so filenames cannot inject terminal/log lines.
    tracing::warn!(batch = %batch.id, outcome = ?report.outcome,
        deferred = ?report.deferred_count, stdout = ?output.stdout,
        stderr = ?output.stderr, "worker problem; retaining local diagnostic evidence");
    for job in &report.jobs {
        tracing::warn!(batch = %batch.id, key = %job.key, path = ?job.admitted_path,
            path_truncated = job.path_truncated, "worker problem file context");
    }
    let saved = persist_report(jobs_dir, &report);
    crate::status_provider::update_status_worker_problem(&format!(
        "batch={} unix={} deferred={:?} first_key={} report_saved={} report={REPORT_NAME}",
        report.batch_id,
        report.observed_unix_seconds,
        report.deferred_count,
        report.jobs.first().map_or("none", |job| job.key.as_str()),
        saved.is_ok(),
    ));
    saved
}

fn persist_report(jobs_dir: &Path, report: &ProblemReport<'_>) -> Result<PathBuf> {
    let encoded = serde_json::to_vec(report)?;
    ensure!(
        encoded.len() <= REPORT_BYTES,
        "worker diagnostic report exceeds its byte limit"
    );
    let staged = jobs_dir.join(STAGING_NAME);
    let final_path = jobs_dir.join(REPORT_NAME);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staged)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    drop(file);
    // Both files are in the already-excluded jobs directory. A failed write or
    // replacement leaves the last final report intact; neither file is replayed.
    fs::rename(&staged, &final_path)?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::super::job_dispatch::JobSpec;
    use super::*;
    use std::io::{self, Cursor};

    #[test]
    fn tail_is_bounded_and_accounts_for_every_discarded_byte() {
        let mut tail = Tail::default();
        let mut all = Vec::new();
        for size in [0, 7, 8192, TAIL_BYTES, 17, TAIL_BYTES * 3, 2] {
            let bytes: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
            all.extend_from_slice(&bytes);
            tail.push(&bytes);
            assert!(tail.bytes.len() <= TAIL_BYTES);
            assert_eq!(
                tail.bytes.iter().copied().collect::<Vec<_>>(),
                all[all.len().saturating_sub(TAIL_BYTES)..]
            );
            let snapshot = tail.snapshot();
            assert_eq!(snapshot.total_bytes, all.len() as u64);
            assert_eq!(
                snapshot.dropped_bytes,
                all.len().saturating_sub(TAIL_BYTES) as u64
            );
        }
    }

    #[test]
    fn both_streams_drain_to_eof_after_their_retained_tails_fill() -> Result<()> {
        let pool = Arc::new(Semaphore::new(2));
        let mut stdout = vec![b'a'; TAIL_BYTES * 8];
        stdout.extend_from_slice(b"stdout-final");
        let mut stderr = vec![b'b'; TAIL_BYTES * 9];
        stderr.extend_from_slice(b"stderr-final");
        let output = OutputReservation::from_pool(Arc::clone(&pool))?
            .start_readers(Cursor::new(stdout.clone()), Cursor::new(stderr.clone()))?
            .finish();
        assert!(!output.incomplete());
        assert!(output.stdout.text.ends_with("stdout-final"));
        assert!(output.stderr.text.ends_with("stderr-final"));
        assert_eq!(output.stdout.total_bytes, stdout.len() as u64);
        assert_eq!(output.stderr.total_bytes, stderr.len() as u64);
        assert_eq!(output.stdout.dropped_bytes, (stdout.len() - TAIL_BYTES) as u64);
        assert_eq!(pool.available_permits(), 2);
        Ok(())
    }

    #[test]
    fn interrupted_reads_retry_but_broken_streams_remain_explicitly_incomplete() -> Result<()> {
        struct Broken(u8);
        impl Read for Broken {
            fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
                self.0 += 1;
                match self.0 {
                    1 => Err(io::ErrorKind::Interrupted.into()),
                    2 => {
                        target[..3].copy_from_slice(b"end");
                        Ok(3)
                    }
                    _ => Err(io::Error::other("failed output read")),
                }
            }
        }
        let pool = Arc::new(Semaphore::new(2));
        let output = OutputReservation::from_pool(Arc::clone(&pool))?
            .start_readers(Broken(0), Cursor::new(Vec::<u8>::new()))?
            .finish();
        assert!(output.incomplete());
        assert_eq!(output.stdout.text, "end");
        assert_eq!(output.stdout.error.as_deref(), Some("failed output read"));
        assert!(output.stderr.complete);
        assert_eq!(pool.available_permits(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn an_unclosed_stream_retains_its_slot_after_the_waiter_times_out() -> Result<()> {
        struct Held(mpsc::Receiver<()>);
        impl Read for Held {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.0.recv().map_err(io::Error::other)?;
                Ok(0)
            }
        }
        let pool = Arc::new(Semaphore::new(2));
        let (release, held) = mpsc::channel();
        let output = OutputReservation::from_pool(Arc::clone(&pool))?
            .start_readers(Held(held), Cursor::new(Vec::<u8>::new()))?
            .finish_with_timeout(Duration::ZERO);
        assert!(output.incomplete());
        assert!(OutputReservation::from_pool(Arc::clone(&pool)).is_err());
        release.send(())?;
        let _released = tokio::time::timeout(Duration::from_secs(2), pool.acquire_many_owned(2))
            .await??;
        Ok(())
    }

    #[test]
    fn report_names_deferred_identities_and_preserves_previous_evidence() -> Result<()> {
        let root = tempfile::tempdir()?;
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: (0..140)
                .map(|n| JobSpec {
                    operation: JobOperation::Reconcile,
                    volume_id: 7,
                    file_id: 0xffff_0000_0000_0000 + n,
                    path: PathBuf::from(format!("C:\\{}\\file{n}.txt", "📄\n".repeat(600))),
                    max_bytes: None,
                    max_chars: None,
                    file_size: 7,
                })
                .collect(),
            reset_volumes: Vec::new(),
        };
        let key = DocKey::from_parts(7, batch.jobs[139].file_id);
        let output = CapturedOutput {
            stdout: Tail::default().snapshot(),
            stderr: Tail::default().snapshot(),
        };
        let path = record_problem(root.path(), &batch, "deferred", Some(&[key]), &output)?;
        let report: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
        assert_eq!(report["deferred_count"], 1);
        assert_eq!(report["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(report["jobs"][0]["file_reference"], "0xffff00000000008b");
        assert_eq!(report["jobs"][0]["path_truncated"], true);
        // A later fatal attempt has no validated per-file outcome. Preserve
        // that distinction instead of labeling every candidate as deferred.
        record_problem(root.path(), &batch, "receipt missing", None, &output)?;
        let saved = fs::read(&path)?;
        assert!(saved.len() <= REPORT_BYTES);
        let report: serde_json::Value = serde_json::from_slice(&saved)?;
        assert!(report["deferred_count"].is_null());
        assert_eq!(report["jobs"].as_array().unwrap().len(), REPORT_JOBS);
        assert_eq!(report["omitted_jobs"], 12);
        assert_eq!(fs::read_dir(root.path())?.count(), 1);
        fs::create_dir(root.path().join(STAGING_NAME))?;
        assert!(record_problem(root.path(), &batch, "later failure", None, &output).is_err());
        assert_eq!(fs::read(path)?, saved);
        Ok(())
    }

    #[test]
    fn service_command_disables_previews_and_always_requests_warning_diagnostics() {
        let mut command = Command::new("index-worker");
        configure_command(&mut command);
        let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy()).collect();
        assert_eq!(args, ["--preview-chars", "0"]);
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("RUST_LOG")
                && value == Some(std::ffi::OsStr::new("warn"))
        }));
    }
}
