use anyhow::{Context, Result};
use core_types::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};
use tokio::task;
use tracing::{error, info};

// A failed or hung extractor must eventually release the single writer lane.
const MAX_WORKER_DURATION: Duration = Duration::from_secs(5 * 60);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
        self.spawn_index_batch(&IndexBatch {
            id: uuid::Uuid::new_v4(),
            jobs,
            reset_volumes: Vec::new(),
        })
        .await
    }

    /// Run a batch to a successful worker commit before acknowledging it.
    /// Reusing the identifier also reuses the failure artifact on every retry.
    pub async fn spawn_index_batch(&self, work: &IndexBatch) -> Result<()> {
        if work.jobs.is_empty() && work.reset_volumes.is_empty() {
            return Ok(());
        }

        if !self.jobs_dir.exists() {
            tokio::fs::create_dir_all(&self.jobs_dir).await?;
        }

        let batch_id = work.id;
        let job_file_path = self.jobs_dir.join(format!("job_{}.json", batch_id));

        let batch = JobBatch {
            // Version 1 workers ignore operation/reset fields. They must reject
            // this envelope before an old writer can mutate the new index.
            version: 2,
            jobs: work.jobs.clone(),
            reset_volumes: work.reset_volumes.clone(),
        };

        let json = serde_json::to_string_pretty(&batch)?;
        tokio::fs::write(&job_file_path, json).await?;

        info!(
            "Spawning worker for batch {} ({} jobs) using worker_path={}",
            batch_id,
            work.jobs.len(),
            self.worker_path.display()
        );

        let worker_path = self.worker_path.clone();
        let job_file_for_spawn = job_file_path.clone();
        let index_dir_for_spawn = self.index_dir.clone();
        let index_dir_for_log = index_dir_for_spawn.clone();

        let status = task::spawn_blocking(move || -> anyhow::Result<ExitStatus> {
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

            child.wait_for_exit(MAX_WORKER_DURATION)
        })
        .await??;

        if status.success() {
            info!(
                "Worker batch {} completed successfully (status={})",
                batch_id, status
            );
            tokio::fs::remove_file(job_file_path).await.ok();
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

struct WorkerChild {
    child: Child,
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
            let error = dispatcher.spawn_index_batch(&batch).await.unwrap_err();
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
        assert_eq!(saved["version"], serde_json::json!(2));
        assert_eq!(saved["reset_volumes"], serde_json::json!([3]));
        assert_eq!(saved["jobs"], serde_json::json!([]));
        Ok(())
    }
}
