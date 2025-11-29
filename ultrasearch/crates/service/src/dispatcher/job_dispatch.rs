use anyhow::{Context, Result};
use core_types::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::task;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
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

#[derive(Debug, Serialize)]
struct JobBatch {
    version: u32,
    jobs: Vec<JobSpec>,
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
                    .and_then(|p| p.parent().map(|d| d.join("search-index-worker")))
            })
            .unwrap_or_else(|| PathBuf::from("search-index-worker"));

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
        if jobs.is_empty() {
            return Ok(());
        }

        if !self.jobs_dir.exists() {
            tokio::fs::create_dir_all(&self.jobs_dir).await?;
        }

        let batch_id = uuid::Uuid::new_v4();
        let job_file_path = self.jobs_dir.join(format!("job_{}.json", batch_id));

        let batch = JobBatch {
            version: 1,
            jobs: jobs.clone(),
        };

        let json = serde_json::to_string_pretty(&batch)?;
        tokio::fs::write(&job_file_path, json).await?;

        info!(
            "Spawning worker for batch {} ({} jobs)",
            batch_id,
            jobs.len()
        );

        let worker_path = self.worker_path.clone();
        let job_file_for_spawn = job_file_path.clone();
        let index_dir = self.index_dir.clone();

        let status = task::spawn_blocking(move || -> anyhow::Result<std::process::ExitStatus> {
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::io::AsHandle;
                use std::os::windows::process::CommandExt;
                use std::process::{Command, ExitStatus};
                use tracing::warn;

                const CREATE_NO_WINDOW: u32 = 0x08000000;
                let mut child = Command::new(&worker_path)
                    .arg("--job-file")
                    .arg(&job_file_for_spawn)
                    .arg("--index-dir")
                    .arg(&index_dir)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn()
                    .context("failed to spawn worker process")?;

                let handle = child.as_handle();
                if let Err(e) = attach_background_job_object(handle) {
                    warn!("attach_background_job_object failed: {e}");
                }

                let status: ExitStatus = child.wait()?;
                Ok(status)
            }

            #[cfg(not(target_os = "windows"))]
            {
                use std::process::{Command, ExitStatus};
                let status: ExitStatus = Command::new(&worker_path)
                    .arg("--job-file")
                    .arg(&job_file_for_spawn)
                    .arg("--index-dir")
                    .arg(&index_dir)
                    .spawn()
                    .context("failed to spawn worker process")?
                    .wait()?;
                Ok(status)
            }
        })
        .await??;

        if status.success() {
            info!("Worker batch {} completed successfully", batch_id);
            tokio::fs::remove_file(job_file_path).await.ok();
        } else {
            error!("Worker batch {} failed with status: {}", batch_id, status);
        }

        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn attach_background_job_object(handle: std::os::windows::io::BorrowedHandle<'_>) -> Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;

    unsafe {
        let job = CreateJobObjectW(None, None)?;

        // Hard-cap CPU at 20% to stay invisible
        let mut cpu_info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: 2000 },
        };
        SetInformationJobObject(
            job,
            JobObjectCpuRateControlInformation,
            &mut cpu_info as *mut _ as *const _,
            size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        )
        .ok()
        .ok_or_else(|| anyhow::anyhow!("SetInformationJobObject failed"))?;

        AssignProcessToJobObject(job, HANDLE(handle.as_raw_handle() as isize))
            .ok()
            .ok_or_else(|| anyhow::anyhow!("AssignProcessToJobObject failed"))?;
    }

    Ok(())
}
