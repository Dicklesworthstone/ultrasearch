use crate::dispatcher::job_dispatch::JobSpec;
use crate::meta_ingest::ingest_with_paths;
use crate::scheduler_runtime::{content_job_from_meta, enqueue_content_job};
use crate::status_provider::{update_status_last_commit, update_status_volumes};
use anyhow::Result;
use core_types::FileMeta;
use core_types::config::AppConfig;
use ipc::VolumeStatus;
#[cfg(any())]
use meta_index::{open_or_create_index, open_reader};
use ntfs_watcher::{
    FileEvent, JournalCursor, NtfsError, VolumeInfo, discover_volumes, enumerate_mft, tail_usn,
};
#[cfg(any())]
use std::collections::HashMap;
#[cfg(any())]
use std::fs;
#[cfg(any())]
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(any())]
use tantivy::DocAddress;
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
        tracing::info!(
            "Volume list empty in config; skipping initial scan until a drive is selected."
        );
        Vec::new()
    } else {
        all_volumes
            .into_iter()
            .filter(|v| {
                v.drive_letters.iter().any(|l| {
                    let mount = format!("{}:\\", l);
                    cfg.volumes.contains(&mount)
                })
            })
            .collect()
    };

    if volumes.is_empty() {
        tracing::info!("No volumes matched configuration.");
        update_status_volumes(Vec::new());
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

                let (content_jobs, content_bytes) = build_content_jobs(&metas, cfg);

                let count = metas.len() as u64;
                let total_bytes: u64 = metas.iter().map(|m| m.size).sum();
                tracing::info!(guid = %volume.guid_path, files = count, "ingesting metadata batch into meta-index");
                match ingest_with_paths(&cfg.paths, metas, None) {
                    Ok(_) => tracing::info!("Successfully ingested {} files.", count),
                    Err(e) => tracing::error!("Failed to ingest files: {}", e),
                }

                let pending_files = content_jobs.len() as u64;
                jobs.extend(content_jobs);

                status.push(VolumeStatus {
                    volume: volume.id,
                    indexed_files: count,
                    indexed_bytes: total_bytes,
                    pending_files,
                    pending_bytes: content_bytes,
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
            return Ok(());
        }
        Err(err) => {
            tracing::warn!(error = %err, "change watcher: failed to discover volumes");
            return Ok(());
        }
    };

    if volumes.is_empty() {
        tracing::info!("change watcher: no volumes matched configuration");
        update_status_volumes(Vec::new());
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
                        let mut dropped = 0;
                        for job in jobs {
                            if !enqueue_content_job(job) {
                                dropped += 1;
                            }
                        }
                        tracing::debug!(
                            volume = vol.id,
                            events = events.len(),
                            dropped,
                            "change watcher enqueued {} jobs (dropped {})",
                            events.len(),
                            dropped
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

fn build_content_jobs(metas: &[FileMeta], cfg: &AppConfig) -> (Vec<JobSpec>, u64) {
    let mut total_bytes = 0u64;
    let jobs = metas
        .iter()
        .filter_map(|meta| {
            if let Some(job) = content_job_from_meta(meta, &cfg.extract) {
                total_bytes = total_bytes.saturating_add(meta.size);
                Some(job)
            } else {
                None
            }
        })
        .collect();
    (jobs, total_bytes)
}

fn filter_volumes(cfg: AppConfig, all_volumes: Vec<VolumeInfo>) -> Vec<VolumeInfo> {
    if cfg.volumes.is_empty() {
        tracing::info!("Volume filter empty; watcher will stay idle until drives are selected.");
        Vec::new()
    } else {
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
#[cfg(any())]
pub async fn watch_polling(cfg: AppConfig) -> Result<()> {
    tracing::info!("change watcher: starting polling fallback");
    let mut last_seen: HashMap<core_types::DocKey, i64> = HashMap::new();
    let mut ticker = interval(Duration::from_secs(30));

    loop {
        ticker.tick().await;
        let cfg_clone = cfg.clone();
        let mut seen_clone = last_seen.clone();
        let res = tokio::task::spawn_blocking(move || {
            detect_changed_files(&cfg_clone, &mut seen_clone).map(|jobs| (jobs, seen_clone))
        })
        .await;

        match res {
            Ok(Ok((jobs, updated_seen))) => {
                if !jobs.is_empty() {
                    for job in jobs {
                        let _ = enqueue_content_job(job);
                    }
                }
                // Update last_seen only on success
                last_seen = updated_seen;
            }
            Ok(Err(err)) => tracing::warn!("polling fallback error: {err}"),
            Err(join_err) => tracing::warn!("polling fallback task panicked: {join_err}"),
        };
    }
}

#[cfg(any())]
#[cfg(any())]
fn detect_changed_files(
    cfg: &AppConfig,
    last_seen: &mut HashMap<core_types::DocKey, i64>,
) -> Result<Vec<JobSpec>> {
    let index_path = Path::new(&cfg.paths.meta_index);
    if !index_path.exists() {
        return Ok(Vec::new());
    }

    let meta = open_or_create_index(index_path)?;
    let reader = open_reader(&meta)?;
    let searcher = reader.searcher();

    let mut changed = Vec::new();

    for (segment_ord, segment_reader) in searcher.segment_readers().iter().enumerate() {
        let alive = segment_reader.alive_bitset();
        let max_doc = segment_reader.max_doc();
        for doc_id in 0..max_doc {
            if let Some(bits) = alive
                && !bits.is_alive(doc_id)
            {
                continue;
            }
            let addr = DocAddress {
                segment_ord: segment_ord as u32,
                doc_id,
            };
            let doc = searcher.doc(addr)?;
            if let Some(meta_doc) = meta_index::tiers::doc_to_meta(&doc, &meta.fields)
                && let Some(path) = &meta_doc.path
            {
                let meta_fs = match fs::metadata(path) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let current_mtime = meta_fs
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(meta_doc.modified);

                let prev = *last_seen.get(&meta_doc.key).unwrap_or(&meta_doc.modified);
                if current_mtime > prev
                    && let Some(job) = content_job_from_meta(
                        &FileMeta {
                            key: meta_doc.key,
                            volume: meta_doc.volume,
                            parent: None,
                            name: meta_doc.name.clone(),
                            ext: meta_doc.ext.clone(),
                            path: Some(path.clone()),
                            size: meta_fs.len(),
                            created: meta_doc.created,
                            modified: current_mtime,
                            flags: core_types::FileFlags::empty(),
                        },
                        &cfg.extract,
                    )
                {
                    changed.push(job);
                }

                last_seen.insert(meta_doc.key, current_mtime);
            }
        }
    }

    Ok(changed)
}
