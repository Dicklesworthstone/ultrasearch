use anyhow::{Context, Result};
use core_types::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::task;
use tracing::{error, info};

// A failed or hung extractor must eventually release the single writer lane.
const MAX_WORKER_DURATION: Duration = Duration::from_secs(5 * 60);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);

// Runtime replacement must not create a second writer lane while a cancelled
// dispatch still owns a detached blocking child. The permit moves with that
// child and covers both legacy and durable batches across scheduler instances.
static WORKER_LANE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

async fn acquire_worker_lane() -> Result<tokio::sync::OwnedSemaphorePermit> {
    Arc::clone(WORKER_LANE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))))
        .acquire_owned()
        .await
        .context("index worker lane is closed")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOperation {
    #[default]
    Upsert,
    /// Reconcile a journal observation with current filesystem state, including
    /// removing a document when the observed file no longer occupies the path.
    Reconcile,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default)]
    pub operation: JobOperation,
    pub volume_id: u16,
    pub file_id: u64,
    pub path: PathBuf,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub file_size: u64,
}

/// One replayable worker transaction. Its identifier remains stable on retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexBatch {
    pub id: uuid::Uuid,
    pub jobs: Vec<JobSpec>,
    pub reset_volumes: Vec<u16>,
}

#[derive(Debug, Serialize)]
struct JobBatch {
    version: u32,
    id: uuid::Uuid,
    jobs: Vec<JobSpec>,
    reset_volumes: Vec<u16>,
}

pub struct JobDispatcher {
    worker_path: PathBuf,
    jobs_dir: PathBuf,
    index_dir: PathBuf,
}

impl JobDispatcher {
    pub fn new(cfg: &AppConfig) -> Self {
        let mut worker_path = std::env::var("ULTRASEARCH_WORKER_PATH")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("index-worker")))
            })
            .unwrap_or_else(|| PathBuf::from("index-worker"));

        if cfg!(windows) && worker_path.extension().is_none() {
            worker_path.set_extension("exe");
        }

        Self {
            worker_path,
            jobs_dir: PathBuf::from(&cfg.paths.jobs_dir),
            index_dir: PathBuf::from(&cfg.paths.content_index),
        }
    }

    pub async fn spawn_batch(&self, jobs: Vec<JobSpec>) -> Result<()> {
        self.spawn_index_batch(
            &IndexBatch {
                id: uuid::Uuid::new_v4(),
                jobs,
                reset_volumes: Vec::new(),
            },
            None,
        )
        .await
    }

    /// Run a batch to a successful worker commit before acknowledging it.
    /// Reusing the identifier also reuses the failure artifact on every retry.
    pub async fn spawn_index_batch(
        &self,
        work: &IndexBatch,
        mutation_lease: Option<Arc<std::fs::File>>,
    ) -> Result<()> {
        anyhow::ensure!(!work.id.is_nil(), "worker batch identity must not be nil");
        let worker_lane = acquire_worker_lane().await?;

        let batch_id = work.id;
        let job_file_path = self.jobs_dir.join(format!("job_{}.json", batch_id));

        let batch = JobBatch {
            // Version 4 workers can atomically record deferred file obligations.
            // Earlier workers must reject this envelope before acknowledging it.
            version: 4,
            id: batch_id,
            jobs: work.jobs.clone(),
            reset_volumes: work.reset_volumes.clone(),
        };

        let json = serde_json::to_string_pretty(&batch)?;

        info!(
            "Spawning worker for batch {} ({} jobs) using worker_path={}",
            batch_id,
            work.jobs.len(),
            self.worker_path.display()
        );

        let worker_path = self.worker_path.clone();
        let jobs_dir_for_spawn = self.jobs_dir.clone();
        let job_file_for_spawn = job_file_path.clone();
        let index_dir_for_spawn = self.index_dir.clone();
        let index_dir_for_log = index_dir_for_spawn.clone();

        let status = task::spawn_blocking(move || -> anyhow::Result<ExitStatus> {
            // File writes and cleanup share the blocking owner's lifetime.
            // Cancelling an async filesystem wrapper could otherwise detach
            // a late write/removal that races the next same-ID retry artifact.
            std::fs::create_dir_all(&jobs_dir_for_spawn)?;
            std::fs::write(&job_file_for_spawn, json)?;
            if !worker_path.exists() {
                error!("worker binary missing at {}", worker_path.display());
                anyhow::bail!("worker binary missing at {}", worker_path.display());
            }

            let mut command = std::process::Command::new(&worker_path);
            command
                .arg("--job-file")
                .arg(&job_file_for_spawn)
                .arg("--index-dir")
                .arg(&index_dir_for_spawn);

            #[cfg(target_os = "windows")]
            let job = {
                use std::os::windows::process::CommandExt;
                use windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

                // The worker cannot open an index before assignment to the
                // kill-on-close job. Even service death in this small startup
                // interval cannot leave an unowned worker writing the index.
                command.creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0);
                create_background_job_object()?
            };

            let mut child = WorkerChild {
                child: command.spawn().context("failed to spawn worker process")?,
                _mutation_lease: mutation_lease,
                _worker_lane: Some(worker_lane),
            };

            #[cfg(target_os = "windows")]
            {
                use std::os::windows::io::{AsHandle, AsRawHandle};
                use windows::Win32::Foundation::HANDLE;
                use windows::Win32::System::JobObjects::AssignProcessToJobObject;

                // Keep `job` owned until this child has exited. Closing the
                // service's handles after a crash kills all attached workers.
                unsafe {
                    AssignProcessToJobObject(
                        HANDLE(job.as_raw_handle() as isize),
                        HANDLE(child.child.as_handle().as_raw_handle() as isize),
                    )
                }
                .context("failed to contain worker in its job object")?;
                resume_worker_thread(child.child.id())?;
            }

            let status = child.wait_for_exit(MAX_WORKER_DURATION)?;
            if status.success() {
                // An empty batch also needs a real receipt. Neither successful
                // exit alone nor a receipt left by a failed attempt is an ACK.
                validate_worker_commit(&index_dir_for_spawn, batch_id).with_context(|| {
                    format!(
                        "worker batch {batch_id} exited successfully without its committed receipt; retained job file {}",
                        job_file_for_spawn.display()
                    )
                })?;
                // Best-effort cleanup remains inside the owner of the reaped
                // child and its permit; a later retry cannot overtake it.
                std::fs::remove_file(&job_file_for_spawn).ok();
            }
            Ok(status)
        })
        .await??;

        if status.success() {
            info!(
                "Worker batch {} completed successfully (status={})",
                batch_id, status
            );
        } else {
            error!(
                "Worker batch {} failed with status: {} (job_file={}, index_dir={})",
                batch_id,
                status,
                job_file_path.display(),
                index_dir_for_log.display()
            );
            anyhow::bail!(
                "worker batch {batch_id} failed with status {status}; retained job file {}",
                job_file_path.display()
            );
        }

        Ok(())
    }
}

fn validate_worker_commit(index_dir: &Path, batch_id: uuid::Uuid) -> Result<()> {
    // Do not create an index here: an absent committed index is a failed ACK.
    let index = tantivy::Index::open_in_dir(index_dir)?;
    content_index::validate_schema(&index)?;
    let committed = content_index::committed_batch(&index)?;
    anyhow::ensure!(
        committed == Some(batch_id),
        "content index receipt {committed:?} does not match worker batch {batch_id}"
    );
    Ok(())
}

struct WorkerChild {
    child: Child,
    // A cancelled async dispatcher cannot cancel an already running blocking
    // task. Keep session ownership until this child has exited or been reaped,
    // so another ingestion session cannot start behind a detached old writer.
    _mutation_lease: Option<Arc<std::fs::File>>,
    _worker_lane: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl WorkerChild {
    fn wait_for_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let started = Instant::now();
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .context("failed to poll index worker")?
            {
                return Ok(status);
            }
            if started.elapsed() >= timeout {
                self.child
                    .kill()
                    .context("failed to stop timed out worker")?;
                self.child
                    .wait()
                    .context("failed to reap timed out worker")?;
                anyhow::bail!(
                    "index worker exceeded its {} second deadline and was stopped",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(WORKER_POLL_INTERVAL.min(timeout - started.elapsed().min(timeout)));
        }
    }
}

impl Drop for WorkerChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            // Every error path owns its child until it has been stopped and
            // reaped, so a later replay cannot race a leftover writer.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(target_os = "windows")]
fn create_background_job_object() -> Result<std::os::windows::io::OwnedHandle> {
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows::Win32::System::JobObjects::*;

    unsafe {
        let job = CreateJobObjectW(None, None)?;
        let owned = OwnedHandle::from_raw_handle(job.0 as *mut std::ffi::c_void);

        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;

        // Hard-cap CPU at 20% to stay invisible
        let cpu_info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 2000 },
        };
        SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            &cpu_info as *const _ as *const _,
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        )?;
        Ok(owned)
    }
}

#[cfg(target_os = "windows")]
fn resume_worker_thread(process_id: u32) -> Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?;
        let _snapshot_owner = OwnedHandle::from_raw_handle(snapshot.0 as *mut std::ffi::c_void);
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        Thread32First(snapshot, &mut entry)?;
        loop {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)?;
                let _thread_owner = OwnedHandle::from_raw_handle(thread.0 as *mut std::ffi::c_void);
                let previous_count = ResumeThread(thread);
                anyhow::ensure!(
                    previous_count == 1,
                    "failed to resume newly suspended worker thread (suspend count {previous_count})"
                );
                return Ok(());
            }
            if Thread32Next(snapshot, &mut entry).is_err() {
                break;
            }
        }
    }
    anyhow::bail!("new worker process has no resumable initial thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn deadline_stops_and_reaps_its_child_before_returning_failure() -> Result<()> {
        // This exercises process ownership only; it is not a worker or NTFS
        // integration test. A zero deadline makes the outcome deterministic.
        let mut child = WorkerChild {
            child: std::process::Command::new("sleep").arg("30").spawn()?,
            _mutation_lease: None,
            _worker_lane: None,
        };
        let error = child.wait_for_exit(Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        let status = child
            .child
            .try_wait()?
            .expect("child must already be reaped");
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn durable_batch_serialization_preserves_identity_and_operations() -> Result<()> {
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: vec![JobSpec {
                operation: JobOperation::Delete,
                volume_id: 7,
                file_id: 91,
                path: PathBuf::new(),
                max_bytes: None,
                max_chars: None,
                file_size: 0,
            }],
            reset_volumes: vec![7],
        };
        let encoded = serde_json::to_vec(&batch)?;
        let restored: IndexBatch = serde_json::from_slice(&encoded)?;
        assert_eq!(restored.id, batch.id);
        assert_eq!(restored.reset_volumes, vec![7]);
        assert_eq!(restored.jobs[0].operation, JobOperation::Delete);
        assert_eq!(restored.jobs[0].file_id, 91);
        Ok(())
    }

    #[tokio::test]
    async fn failed_reset_only_batch_reuses_one_replay_artifact() -> Result<()> {
        // Keep failure evidence: no worker binary or native runtime is faked.
        let root = tempfile::tempdir()?.keep();
        let dispatcher = JobDispatcher {
            worker_path: root.join("missing-index-worker"),
            jobs_dir: root.join("jobs"),
            index_dir: root.join("content"),
        };
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: Vec::new(),
            reset_volumes: vec![3],
        };
        for _ in 0..2 {
            let error = dispatcher
                .spawn_index_batch(&batch, None)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("worker binary missing"));
        }
        let files =
            std::fs::read_dir(&dispatcher.jobs_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(files.len(), 1, "retries must not accumulate batch files");
        assert_eq!(
            files[0].file_name().to_string_lossy(),
            format!("job_{}.json", batch.id)
        );
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(files[0].path())?)?;
        assert_eq!(saved["version"], serde_json::json!(4));
        assert_eq!(saved["id"], serde_json::json!(batch.id));
        assert_eq!(saved["reset_volumes"], serde_json::json!([3]));
        assert_eq!(saved["jobs"], serde_json::json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn empty_durable_batch_requires_a_worker_and_retains_its_identity() -> Result<()> {
        let root = tempfile::tempdir()?.keep();
        let dispatcher = JobDispatcher {
            worker_path: root.join("missing-index-worker"),
            jobs_dir: root.join("jobs"),
            index_dir: root.join("content"),
        };
        let batch = IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs: Vec::new(),
            reset_volumes: Vec::new(),
        };
        let error = dispatcher
            .spawn_index_batch(&batch, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("worker binary missing"));
        let path = dispatcher.jobs_dir.join(format!("job_{}.json", batch.id));
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        assert_eq!(saved["version"], serde_json::json!(4));
        assert_eq!(saved["id"], serde_json::json!(batch.id));
        assert_eq!(saved["jobs"], serde_json::json!([]));
        assert_eq!(saved["reset_volumes"], serde_json::json!([]));
        assert!(!dispatcher.index_dir.exists());
        Ok(())
    }

    #[test]
    fn acknowledgement_requires_the_actual_matching_committed_index() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("content");
        let requested = uuid::Uuid::new_v4();
        assert!(validate_worker_commit(&path, requested).is_err());
        assert!(!path.exists(), "ACK validation must not create an index");
        let index = content_index::open_or_create(&path)?;
        let mut writer = content_index::create_writer(
            &index,
            &content_index::WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        assert!(validate_worker_commit(&path, requested).is_err());
        content_index::commit_batch(&mut writer, uuid::Uuid::new_v4())?;
        assert!(validate_worker_commit(&path, requested).is_err());
        content_index::commit_partial_batch(&mut writer, requested)?;
        assert!(validate_worker_commit(&path, requested).is_err());
        content_index::commit_batch(&mut writer, requested)?;
        validate_worker_commit(&path, requested)?;
        // Deferrals are durable output, not a process failure. Their membership
        // in the pending intent is checked by the service before checkpointing.
        content_index::commit_batch_with_deferred(
            &mut writer,
            requested,
            &[core_types::DocKey::from_parts(7, 42)],
        )?;
        validate_worker_commit(&path, requested)?;
        // An external plain commit no longer attests to this batch, even if its
        // previously stamped ingestion-generation sidecar was left untouched.
        writer.commit()?;
        assert!(validate_worker_commit(&path, requested).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_owner_blocks_new_sessions_and_worker_successors_until_reaped() -> Result<()>
    {
        // This is a process/lock ownership regression, not a simulated worker
        // or NTFS integration test. Channels place cancellation after the real
        // blocking child owner has acquired the lease, without timing sleeps.
        let root = tempfile::tempdir()?;
        let lock_path = root.path().join("ingestion.lock");
        let lease = Arc::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)?,
        );
        lease.try_lock()?;
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)?;
        let worker_lane = acquire_worker_lane().await?;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            task::spawn_blocking(move || -> Result<()> {
                let mut child = WorkerChild {
                    child: std::process::Command::new("cat")
                        .stdin(std::process::Stdio::piped())
                        .spawn()?,
                    _mutation_lease: Some(lease),
                    _worker_lane: Some(worker_lane),
                };
                let _ = started_tx.send(());
                release_rx.recv()?;
                let error = child.wait_for_exit(Duration::ZERO).unwrap_err();
                assert!(error.to_string().contains("deadline"));
                drop(child);
                let _ = finished_tx.send(());
                Ok(())
            })
            .await?
        });
        started_rx.await?;
        owner.abort();
        assert!(owner.await.unwrap_err().is_cancelled());
        assert!(
            contender.try_lock().is_err(),
            "detached child must retain the session lock"
        );
        let mut successor = Box::pin(acquire_worker_lane());
        {
            use std::future::Future;
            use std::task::{Context, Waker};
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                successor.as_mut().poll(&mut context).is_pending(),
                "a replacement scheduler must wait for the old child owner"
            );
        }
        release_tx.send(())?;
        finished_rx.await?;
        let _successor_permit = successor.await?;
        contender.try_lock()?;
        Ok(())
    }
}
