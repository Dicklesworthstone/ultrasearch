use crate::dispatcher::job_dispatch::JobSpec;
use crate::meta_ingest::ingest_with_paths;
use crate::scheduler_runtime::{content_job_from_meta, enqueue_content_job};
use crate::status_provider::{update_status_last_commit, update_status_volumes};
use anyhow::Result;
use core_types::{DocKey, FileFlags, FileMeta};
use core_types::config::AppConfig;
use ipc::VolumeStatus;
use meta_index::{open_or_create_index, open_reader};
use ntfs_watcher::{
    FileEvent, JournalCursor, NtfsError, VolumeInfo, discover_volumes, enumerate_mft, tail_usn,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, interval};

pub fn scan_volumes(cfg: &AppConfig) -> Result<Vec<JobSpec>> {
    tracing::info!("Starting volume scan...");
    let all_volumes = match discover_volumes() {
        Ok(v) if v.is_empty() => {
            tracing::info!("no NTFS volumes discovered");
            return Ok(Vec::new());
        }
        Ok(v) => {
            tracing::info!("Discovered {} NTFS volumes total.", v.len());
            v
        }
        Err(NtfsError::NotSupported) => {
            tracing::info!("platform does not support NTFS watcher");
            return Ok(Vec::new());
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to discover volumes");
            return Ok(Vec::new());
        }
    };

    // Filter based on config
    let volumes: Vec<_> = if cfg.volumes.is_empty() {
        // If no volumes specified, maybe default to all?
        // Or if onboarding wizard sets it, we respect it.
        // "First-Run" usually sets it.
        // If empty, we scan nothing? Or all?
        // Let's default to ALL if empty for backward compat/simplicity,
        // OR assume empty means "configured to scan nothing".
        // The Wizard sets "Select Drives" (defaults to all fixed).
        // So if config is empty, it might mean "not set up yet".
        // But `bootstrap.rs` used to scan all.
        // Let's keep "Scan All" if config.volumes is empty to avoid breaking existing behavior.
        all_volumes
    } else {
        all_volumes
            .into_iter()
            .filter(|v| {
                // Check if any drive letter matches config
                // Config has "C:\" or "D:\"
                // VolumeInfo has `drive_letters` vec['C']
                v.drive_letters.iter().any(|l| {
                    let mount = format!("{}:\\", l);
                    cfg.volumes.contains(&mount)
                })
            })
            .collect()
    };

    if volumes.is_empty() {
        tracing::info!("No volumes matched configuration.");
        return Ok(Vec::new());
    }

    let mut jobs: Vec<JobSpec> = Vec::new();
    let mut status = Vec::with_capacity(volumes.len());

    for volume in volumes {
        tracing::info!(guid = %volume.guid_path, letters = ?volume.drive_letters, "enumerating MFT for volume");
        match enumerate_mft(&volume) {
            Ok(metas) => {
                if metas.is_empty() {
                    tracing::info!(guid = %volume.guid_path, "no entries found during MFT enumeration");
                    continue;
                }

                let content_jobs = build_content_jobs(&metas, cfg);

                let count = metas.len() as u64;
                tracing::info!(guid = %volume.guid_path, files = count, "ingesting metadata batch into meta-index");
                match ingest_with_paths(&cfg.paths, metas, None) {
                    Ok(_) => tracing::info!("Successfully ingested {} files.", count),
                    Err(e) => tracing::error!("Failed to ingest files: {}", e),
                }

                jobs.extend(content_jobs);

                status.push(VolumeStatus {
                    volume: volume.id,
                    indexed_files: count,
                    pending_files: 0,
                    last_usn: None,
                    journal_id: None,
                });

                update_status_last_commit(Some(unix_timestamp_secs()));
            }
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("Access is denied") || msg.contains("privilege") {
                    tracing::error!(
                        guid = %volume.guid_path,
                        "CRITICAL: Failed to enumerate MFT due to permissions. Please run the application as Administrator."
                    );
                } else {
                    tracing::warn!(
                        guid = %volume.guid_path,
                        error = %err,
                        "failed to enumerate MFT; skipping volume"
                    );
                }
            }
        }
    }

    if !status.is_empty() {
        update_status_volumes(status);
    }

    Ok(jobs)
}

/// Spawn a background task that tails the USN journal (where available) and enqueues content jobs.
pub async fn watch_changes(cfg: AppConfig) -> Result<()> {
    let volumes = match discover_volumes() {
        Ok(v) if v.is_empty() => {
            tracing::info!("change watcher: no NTFS volumes discovered");
            return Ok(());
        }
        Ok(v) => filter_volumes(cfg.clone(), v),
        Err(NtfsError::NotSupported) => {
            tracing::info!("change watcher: USN not supported; falling back to polling.");
            return watch_polling(cfg).await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "change watcher: failed to discover volumes");
            return watch_polling(cfg).await;
        }
    };

    if volumes.is_empty() {
        tracing::info!("change watcher: no volumes matched configuration");
        return Ok(());
    }

    // Initialize cursors per volume (start at 0).
    let mut cursors = volumes
        .iter()
        .map(|v| {
            (
                v.id,
                JournalCursor {
                    last_usn: 0,
                    journal_id: 0,
                },
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    let mut ticker = interval(Duration::from_secs(5));
    loop {
        ticker.tick().await;
        for vol in volumes.iter() {
            let cursor = *cursors.get(&vol.id).unwrap_or(&JournalCursor {
                last_usn: 0,
                journal_id: 0,
            });

            match tail_usn(vol, cursor) {
                Ok((events, next)) => {
                    if !events.is_empty() {
                        let jobs = events_to_jobs(&events, &cfg);
                        for job in jobs {
                            if enqueue_content_job(job) {
                                // count handled in scheduler
                            }
                        }
                        tracing::debug!(
                            volume = vol.id,
                            events = events.len(),
                            "change watcher enqueued {} jobs",
                            events.len()
                        );
                    }
                    cursors.insert(vol.id, next);
                }
                Err(NtfsError::GapDetected) => {
                    tracing::warn!("USN gap detected on volume {}; consider rescan", vol.id);
                }
                Err(err) => {
                    tracing::warn!(volume = vol.id, error = %err, "tail_usn failed");
                }
            }
        }
    }
}

fn unix_timestamp_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn build_content_jobs(metas: &[FileMeta], cfg: &AppConfig) -> Vec<JobSpec> {
    metas
        .iter()
        .filter_map(|meta| content_job_from_meta(meta, &cfg.extract))
        .collect()
}

fn filter_volumes(cfg: AppConfig, all_volumes: Vec<VolumeInfo>) -> Vec<VolumeInfo> {
    if cfg.volumes.is_empty() {
        return all_volumes;
    }
    all_volumes
        .into_iter()
        .filter(|v| {
            v.drive_letters.iter().any(|l| {
                let mount = format!("{}:\\", l);
                cfg.volumes.contains(&mount)
            })
        })
        .collect()
}

fn events_to_jobs(events: &[FileEvent], cfg: &AppConfig) -> Vec<JobSpec> {
    let mut out = Vec::new();
    for ev in events {
        match ev {
            FileEvent::Created(meta) => {
                if let Some(job) = content_job_from_meta(meta, &cfg.extract) {
                    out.push(job);
                }
            }
            FileEvent::Renamed { to, .. } => {
                if let Some(job) = content_job_from_meta(to, &cfg.extract) {
                    out.push(job);
                }
            }
            FileEvent::Modified { .. } | FileEvent::AttributesChanged { .. } => {
                // Lacking path/size here; could trigger a lightweight stat in future.
            }
            FileEvent::Deleted(_) => {}
        }
    }
    out
}

/// Polling-based fallback: walk the metadata index and enqueue files whose mtime increased.
pub async fn watch_polling(cfg: AppConfig) -> Result<()> {
    tracing::info!("change watcher: starting polling fallback");
    let mut last_seen: HashMap<DocKey, i64> = HashMap::new();
    let mut ticker = interval(Duration::from_secs(30));

    loop {
        ticker.tick().await;
        match detect_changed_files(&cfg, &mut last_seen) {
            Ok(jobs) if !jobs.is_empty() => {
                for job in jobs {
                    let _ = enqueue_content_job(job);
                }
            }
            Ok(_) => { /* no changes */ }
            Err(err) => tracing::warn!("polling fallback error: {err}"),
        }
    }
}

fn detect_changed_files(
    cfg: &AppConfig,
    last_seen: &mut HashMap<DocKey, i64>,
) -> Result<Vec<JobSpec>> {
    let index_path = Path::new(&cfg.paths.meta_index);
    if !index_path.exists() {
        return Ok(Vec::new());
    }

    let meta = open_or_create_index(index_path)?;
    let reader = open_reader(&meta)?;
    let searcher = reader.searcher();

    let mut changed = Vec::new();

    for segment_reader in searcher.segment_readers() {
        let store = segment_reader.get_store_reader(1024)?;
        let alive = segment_reader.alive_bitset();
        for stored_doc in store.iter(alive)? {
            let stored_doc = stored_doc?;

            let key_u64 = stored_doc
                .get_first(meta.fields.doc_key)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let doc_key = DocKey(key_u64);

            let path: Option<String> = stored_doc
                .get_first(meta.fields.path)
                .and_then(|v| v.as_text())
                .map(|s| s.to_string());
            let recorded_modified = stored_doc
                .get_first(meta.fields.modified)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let path = match path {
                Some(p) => p,
                None => continue,
            };

            // stat current mtime
            let meta_fs = match fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let current_mtime = meta_fs
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(recorded_modified);

            let prev = *last_seen.get(&doc_key).unwrap_or(&recorded_modified);
            if current_mtime > prev {
                if let Some(job) = content_job_from_meta(
                    &FileMeta {
                        key: doc_key,
                        volume: doc_key.volume(),
                        parent: None,
                        name: Path::new(&path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_string(),
                        ext: Path::new(&path)
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s.to_ascii_lowercase()),
                        path: Some(path.clone()),
                        size: meta_fs.len(),
                        created: recorded_modified,
                        modified: current_mtime,
                        flags: FileFlags::empty(),
                    },
                    &cfg.extract,
                ) {
                    changed.push(job);
                }
            }

            last_seen.insert(doc_key, current_mtime);
        }
    }

    Ok(changed)
}
