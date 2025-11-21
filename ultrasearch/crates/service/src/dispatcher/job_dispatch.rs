use anyhow::{Context, Result};
use core_types::config::AppConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
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
        // Assume worker binary is adjacent to service, or in path.
        // For dev, we might use "cargo run" wrapper or explicit path.
        // In release, it's "search-index-worker".
        // We'll look for it in the current exe dir.
        
        let mut worker_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("search-index-worker")))
            .unwrap_or_else(|| PathBuf::from("search-index-worker"));

        // On Windows, add .exe
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

        info!("Spawning worker for batch {} ({} jobs)", batch_id, jobs.len());

        let status = Command::new(&self.worker_path)
            .arg("--job-file")
            .arg(&job_file_path)
            .arg("--index-dir")
            .arg(&self.index_dir)
            .stdout(Stdio::inherit()) // Or piped for logging
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn worker")?
            .wait()
            .await?;

        if status.success() {
            info!("Worker batch {} completed successfully", batch_id);
            // Cleanup
            tokio::fs::remove_file(job_file_path).await.ok();
        } else {
            error!("Worker batch {} failed with status: {}", batch_id, status);
            // Keep job file for debugging? Or move to failed/
        }

        Ok(())
    }
}
