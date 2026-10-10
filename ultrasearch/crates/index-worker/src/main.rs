//! Index worker: extract files and write to the content index.
//!
//! - Single-file extraction (`--path` + volume/file ids)
//! - Ordered JSON jobs (`--job-file`) with upsert/delete and volume reset
//! - Optional Extractous backend toggle via flag or ULTRASEARCH_ENABLE_EXTRACTOUS
//! - Preview or JSON output for debugging
//! - Writes extracted docs into the content index (creates if missing)

use anyhow::{Context, Result};
use clap::Parser;
use content_extractor::{ExtractContext, ExtractorStack};
use content_index::{ContentIndex, IndexWriter, WriterConfig};
use core_types::DocKey;
use dotenvy::dotenv;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::{env, fs};
use tracing::{info, warn};
use uuid::Uuid;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Volume id for the document key (required when --job-file is not supplied).
    #[arg(long)]
    volume_id: Option<u16>,
    /// File reference number (FRN) for the document key (required when --job-file is not supplied).
    #[arg(long)]
    file_id: Option<u64>,
    /// Path to a single file to extract (required if --job-file is not provided).
    #[arg(long)]
    path: Option<PathBuf>,
    /// Path to the content index directory (created if missing).
    #[arg(long)]
    index_dir: PathBuf,
    /// Maximum bytes to read (default 10 MiB).
    #[arg(long, default_value = "10485760")]
    max_bytes: usize,
    /// Maximum characters to keep (default 100k).
    #[arg(long, default_value = "100000")]
    max_chars: usize,
    /// Enable Extractous backend (requires feature extractous_backend).
    #[arg(long, default_value = "false")]
    enable_extractous: bool,
    /// Emit full JSON to stdout instead of a text preview.
    #[arg(long, default_value = "false")]
    json: bool,
    /// Preview length when not using JSON output.
    #[arg(long, default_value = "200")]
    preview_chars: usize,
    /// Optional JSON job file (array of jobs). When set, --path is ignored.
    #[arg(long)]
    job_file: Option<PathBuf>,
    /// Commit after at most N docs (0 = commit once at end).
    #[arg(long, default_value = "0")]
    commit_every: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobSpec {
    volume_id: u16,
    file_id: u64,
    #[serde(default)]
    path: PathBuf,
    #[serde(default)]
    operation: JobOperation,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    file_size: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobOperation {
    #[default]
    Upsert,
    Reconcile,
    Delete,
}

#[derive(Debug, Clone, Deserialize)]
struct JobFile {
    version: u32,
    #[serde(default)]
    id: Option<Uuid>,
    #[serde(default)]
    reset_volumes: Vec<u16>,
    #[serde(default)]
    jobs: Vec<JobSpec>,
}

#[derive(Debug, Serialize)]
struct OutputRecord<'a> {
    volume_id: u16,
    file_id: u64,
    truncated: bool,
    bytes_processed: usize,
    lang: Option<&'a str>,
    content_lang: Option<&'a str>,
    text: &'a str,
}

fn main() -> Result<()> {
    dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Lower process + I/O priority to keep the machine responsive
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, IDLE_PRIORITY_CLASS, SetPriorityClass,
            SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN, THREAD_PRIORITY,
        };
        unsafe {
            let _ = SetPriorityClass(GetCurrentProcess(), IDLE_PRIORITY_CLASS);
            let _ = SetThreadPriority(
                GetCurrentThread(),
                THREAD_PRIORITY(THREAD_MODE_BACKGROUND_BEGIN.0),
            );
        }
    }

    let mut args = Args::parse();

    // Allow env override for Extractous toggle.
    if let Ok(val) = env::var("ULTRASEARCH_ENABLE_EXTRACTOUS") {
        args.enable_extractous = matches!(val.as_str(), "1" | "true" | "TRUE");
    }

    #[cfg(not(feature = "extractous_backend"))]
    if args.enable_extractous {
        warn!(
            "--enable-extractous requested, but this binary was built without the `extractous_backend` feature; proceeding with simple-text extractor only"
        );
    }

    #[cfg(feature = "extractous_backend")]
    if args.enable_extractous && !detect_graalvm() {
        let hint = env::var("ULTRASEARCH_GRAALVM_HINT").unwrap_or_else(|_| {
            "Set GRAALVM_HOME (or JAVA_HOME) to your GraalVM CE 23.x install and ensure bin is on PATH"
                .to_string()
        });
        warn!(
            "Extractous requested but GraalVM was not detected. {}. Falling back to simple-text.",
            hint
        );
        args.enable_extractous = false;
    }

    let stack = ExtractorStack::with_extractous_enabled(args.enable_extractous);

    // Open index writer once for the run.
    let index: ContentIndex = content_index::open_or_create(&args.index_dir)?;
    let mut writer: IndexWriter = content_index::create_writer(&index, &WriterConfig::default())?;

    if let Some(job_file) = args.job_file.clone() {
        let batch = load_jobs(&job_file)?;
        process_batch(&stack, &index, &mut writer, batch, &args)?;
    } else {
        let path = args
            .path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--path is required when --job-file is not provided"))?;
        let volume_id = args.volume_id.ok_or_else(|| {
            anyhow::anyhow!("--volume-id is required when --job-file is not provided")
        })?;
        let file_id = args.file_id.ok_or_else(|| {
            anyhow::anyhow!("--file-id is required when --job-file is not provided")
        })?;

        let single = JobSpec {
            volume_id,
            file_id,
            path: path.clone(),
            operation: JobOperation::Upsert,
            max_bytes: Some(args.max_bytes),
            max_chars: Some(args.max_chars),
            file_size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        };

        process_job(&stack, &index, &mut writer, single, &args)?;
        writer.commit()?;
    }

    Ok(())
}

/// Apply one durable intent. Partial commits carry incomplete receipts; a
/// successful attempt always publishes a fresh completed receipt. Callers must
/// replay every retained intent, including previously completed attempts whose
/// acknowledgement or cross-index checkpoint was interrupted.
fn process_batch(
    stack: &ExtractorStack,
    index: &ContentIndex,
    writer: &mut IndexWriter,
    batch: JobFile,
    args: &Args,
) -> Result<()> {
    validate_job_file(&batch)?;
    let batch_id = batch.id;
    let allow_deferral = batch.version == 4;
    let mut pending = 0usize;
    let mut failed_jobs = 0usize;
    let mut deferred = BTreeSet::new();

    for volume in batch.reset_volumes {
        content_index::delete_volume(writer, &index.fields, volume);
        pending += 1;
        if args.commit_every > 0 && pending >= args.commit_every {
            commit_worker_batch(writer, batch_id, false, &[])?;
            pending = 0;
        }
    }
    for job in batch.jobs {
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        let reconcile = job.operation == JobOperation::Reconcile;
        match process_job(stack, index, writer, job, args) {
            Ok(()) => {
                // Later successful reconciliation or deletion supersedes a
                // failed observation of the same identity in this batch.
                deferred.remove(&key);
            }
            Err(error) if allow_deferral && reconcile && error.is::<DeferredFileFailure>() => {
                deferred.insert(key);
                warn!(%key, error = %format!("{error:#}"),
                    "file extraction deferred; tombstone and retry obligation will commit together");
            }
            Err(error) => {
                failed_jobs += 1;
                warn!("job failed: {error:#}");
            }
        }
        pending += 1;
        if args.commit_every > 0 && pending >= args.commit_every {
            commit_worker_batch(writer, batch_id, false, &[])?;
            pending = 0;
        }
    }
    // Successful durable attempts always publish a final complete receipt, even
    // after a periodic commit consumed the last action or the batch was empty.
    // Failed attempts must never leave completion evidence for their subset.
    if pending > 0 || (batch_id.is_some() && failed_jobs == 0) {
        let deferred: Vec<_> = deferred.into_iter().collect();
        commit_worker_batch(writer, batch_id, failed_jobs == 0, &deferred)?;
    }

    if failed_jobs > 0 {
        anyhow::bail!(
            "batch contained {failed_jobs} failed job(s); successful jobs were committed"
        );
    }
    Ok(())
}

fn commit_worker_batch(
    writer: &mut IndexWriter,
    id: Option<Uuid>,
    complete: bool,
    deferred: &[DocKey],
) -> Result<()> {
    match id {
        Some(id) if complete => {
            content_index::commit_batch_with_deferred(writer, id, deferred)?;
        }
        Some(id) => {
            content_index::commit_partial_batch(writer, id)?;
        }
        None => {
            // A standalone legacy writer cannot attest to journal coverage.
            // Plain commits intentionally clear an existing ingestion receipt.
            writer.commit()?;
        }
    }
    Ok(())
}

#[cfg(feature = "extractous_backend")]
fn detect_graalvm() -> bool {
    use std::process::Command;

    let java_bin = env::var("GRAALVM_HOME")
        .or_else(|_| env::var("JAVA_HOME"))
        .map(|home| {
            let mut p = std::path::PathBuf::from(home);
            p.push("bin");
            p.push(if cfg!(windows) { "java.exe" } else { "java" });
            p
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("java"));

    if let Ok(out) = Command::new(java_bin).arg("-version").output() {
        let combined = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase()
            + &String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
        return combined.contains("graalvm");
    }
    false
}

fn load_jobs(job_file: &PathBuf) -> Result<JobFile> {
    let file = fs::File::open(job_file)
        .with_context(|| format!("cannot open job file: {}", job_file.display()))?;
    // Prefer structured batch; fall back to legacy array for compatibility.
    match serde_json::from_reader::<_, JobFile>(&file) {
        Ok(batch) => {
            validate_job_file(&batch)?;
            Ok(batch)
        }
        Err(_) => {
            let file = fs::File::open(job_file)
                .with_context(|| format!("cannot re-open job file: {}", job_file.display()))?;
            let jobs: Vec<JobSpec> = serde_json::from_reader(file).with_context(|| {
                format!("failed to parse legacy job array: {}", job_file.display())
            })?;
            if jobs.is_empty() {
                anyhow::bail!("legacy job array is empty");
            }
            validate_legacy_jobs(&jobs)?;
            Ok(JobFile {
                version: 1,
                id: None,
                reset_volumes: Vec::new(),
                jobs,
            })
        }
    }
}

fn validate_job_file(batch: &JobFile) -> Result<()> {
    anyhow::ensure!(
        matches!(batch.version, 1..=4),
        "unsupported job file version {}",
        batch.version
    );
    if batch.version >= 3 {
        anyhow::ensure!(
            batch.id.is_some_and(|id| !id.is_nil()),
            "job file version 3 or newer requires a nonnil batch id"
        );
    } else {
        anyhow::ensure!(
            batch.id.is_none(),
            "batch commit receipts require job file version 3"
        );
        anyhow::ensure!(
            !batch.jobs.is_empty() || !batch.reset_volumes.is_empty(),
            "job file contains no jobs or volume resets"
        );
    }
    // Version 4 permits completed batches with durable file-level omissions.
    // Older workers reject it rather than silently losing those obligations.
    if batch.version == 1 {
        anyhow::ensure!(
            batch.reset_volumes.is_empty(),
            "volume resets require job file version 2"
        );
        validate_legacy_jobs(&batch.jobs)?;
    }
    let mut reconcile_keys = BTreeSet::new();
    for job in &batch.jobs {
        validate_job(job)?;
        if batch.version == 4 && job.operation == JobOperation::Reconcile {
            reconcile_keys.insert(DocKey::from_parts(job.volume_id, job.file_id));
            anyhow::ensure!(
                reconcile_keys.len() <= content_index::MAX_DEFERRED_KEYS,
                "job batch exceeds the maximum number of deferrable identities"
            );
        }
    }
    Ok(())
}

fn validate_legacy_jobs(jobs: &[JobSpec]) -> Result<()> {
    anyhow::ensure!(
        jobs.iter().all(|job| job.operation == JobOperation::Upsert),
        "reconcile and delete operations require job file version 2"
    );
    Ok(())
}

fn validate_job(job: &JobSpec) -> Result<()> {
    if job.operation != JobOperation::Delete {
        anyhow::ensure!(
            !job.path.as_os_str().is_empty(),
            "job path must not be empty"
        );
        anyhow::ensure!(job.path.to_str().is_some(), "path is not valid UTF-8");
    }
    Ok(())
}

/// Only errors from filesystem probing, extraction, or a changing snapshot
/// carry this context. Validation, index mutations, commit and process errors
/// remain batch failures and must never become acknowledged omissions.
#[derive(Debug)]
struct DeferredFileFailure;

impl std::fmt::Display for DeferredFileFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("file requires deferred extraction")
    }
}

fn process_job(
    stack: &ExtractorStack,
    index: &content_index::ContentIndex,
    writer: &mut IndexWriter,
    job: JobSpec,
    args: &Args,
) -> Result<()> {
    validate_job(&job)?;
    let doc_key = DocKey::from_parts(job.volume_id, job.file_id);

    // Invalidating first prevents stale text from surviving failed extraction.
    // The dispatcher retains a failed batch, and journal replay is idempotent.
    content_index::delete_doc(writer, &index.fields, doc_key);
    if job.operation == JobOperation::Delete {
        return Ok(());
    }
    // Choose per-job limits before capturing bytes; capture obeys the same
    // persisted policy as extraction even if the file has grown while queued.
    let max_bytes = job.max_bytes.unwrap_or(args.max_bytes);
    let max_chars = job.max_chars.unwrap_or(args.max_chars);
    #[cfg(windows)]
    let observed = if job.operation == JobOperation::Reconcile {
        native_snapshot::capture(&job, max_bytes)
    } else {
        probe_job_file(&job)
    };
    #[cfg(not(windows))]
    let observed = probe_job_file(&job);
    let Some(before) = observed? else {
        // A queued path may have been renamed, deleted, or replaced since the
        // USN event. Never attribute a replacement file to the original FRN.
        anyhow::ensure!(
            job.operation == JobOperation::Reconcile,
            "file missing or identity changed: {}",
            job.path.display()
        );
        return Ok(());
    };

    let ext_owned = job
        .path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());

    let ctx = ExtractContext {
        path: job
            .path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?,
        max_bytes,
        max_chars,
        ext_hint: ext_owned.as_deref(),
        mime_hint: None,
    };

    info!(
        "extracting {:?} (vol={}, frn={}) with extractous_enabled={} max_bytes={} max_chars={}",
        job.path, job.volume_id, job.file_id, args.enable_extractous, max_bytes, max_chars
    );

    let out = if before.content_readable {
        let extracted = if let Some(captured) = &before.captured {
            match captured {
                Ok(bytes) => stack.extract_bytes(doc_key, &ctx, bytes),
                Err(bytes) => Err(content_extractor::ExtractError::FileTooLarge {
                    bytes: *bytes,
                    max_bytes: max_bytes as u64,
                }
                .into()),
            }
        } else if job.operation == JobOperation::Reconcile {
            // Pathnames can be swapped away and back while their original
            // handle and metadata remain unchanged. Read the verified handle
            // itself so even that race cannot supply another file's bytes.
            stack.extract_file(
                doc_key,
                &ctx,
                before
                    .file
                    .as_ref()
                    .context("verified data handle missing")?,
            )
        } else {
            stack.extract(doc_key, &ctx)
        };
        match extracted {
            Ok(out) => out,
            Err(error)
                if job.operation == JobOperation::Reconcile
                    && matches!(
                        error.downcast_ref::<content_extractor::ExtractError>(),
                        Some(
                            content_extractor::ExtractError::FileTooLarge { .. }
                                | content_extractor::ExtractError::Unsupported(_)
                        )
                    ) =>
            {
                warn!(key = %doc_key, path = %job.path.display(), %error,
                    "content omitted; reconciling metadata and clearing obsolete text");
                omitted_content(doc_key)
            }
            Err(error) => return Err(error.context(DeferredFileFailure)),
        }
    } else {
        warn!(key = %doc_key, path = %job.path.display(),
            "content access denied; reconciling verified metadata and clearing obsolete text");
        omitted_content(doc_key)
    };
    #[cfg(windows)]
    let observed = if before.captured.is_some() {
        // Bytes are already captured. Attribute-only access validates current
        // identity and metadata without requesting another data-read share.
        probe_metadata_file(&job, true)
    } else {
        probe_job_file(&job)
    };
    #[cfg(not(windows))]
    let observed = probe_job_file(&job);
    let Some(after) = observed? else {
        anyhow::ensure!(
            job.operation == JobOperation::Reconcile,
            "file disappeared or identity changed during extraction: {}",
            job.path.display()
        );
        return Ok(());
    };
    if !before.matches(&after) {
        return Err(anyhow::anyhow!(
            "file changed during extraction; retry required: {}",
            job.path.display()
        )
        .context(DeferredFileFailure));
    }
    let lang = out.lang.clone();
    let truncated = out.truncated;
    let bytes_processed = out.bytes_processed;
    let content_lang = out.content_lang.clone();

    info!(
        "extracted bytes={}, truncated={}, lang={:?}, content_lang={:?}",
        bytes_processed, truncated, out.lang, content_lang
    );

    let content_doc = to_content_doc(&job, &after.metadata, out)?;
    // Release probe handles before Tantivy or output can block. The Windows
    // oplock used to capture bytes has already been released before extraction.
    drop(after);
    drop(before);
    content_index::add_content_doc(writer, &index.fields, &content_doc)?;

    if args.json {
        let record = OutputRecord {
            volume_id: job.volume_id,
            file_id: job.file_id,
            truncated,
            bytes_processed,
            lang: lang.as_deref(),
            content_lang: content_lang.as_deref(),
            text: &content_doc.content,
        };
        println!("{}", serde_json::to_string_pretty(&record)?);
    } else {
        let preview = content_doc
            .content
            .chars()
            .take(args.preview_chars)
            .collect::<String>();
        println!("{preview}");
    }
    Ok(())
}

fn omitted_content(key: DocKey) -> content_extractor::ExtractedContent {
    content_extractor::ExtractedContent {
        key,
        text: String::new(),
        lang: None,
        truncated: true,
        content_lang: None,
        bytes_processed: 0,
    }
}

/// Verified metadata plus either a live source handle or an immutable snapshot.
/// Windows Reconcile captures bytes under an oplock and releases all capture
/// handles before CPU extraction; the full FRN still detects later slot reuse.
struct FileSnapshot {
    file: Option<fs::File>,
    captured: Option<CapturedContent>,
    metadata: fs::Metadata,
    modified: std::time::SystemTime,
    content_readable: bool,
    #[cfg(windows)]
    identity: WindowsFileIdentity,
}

/// Successful bounded bytes, or the observed byte count exceeding the policy.
type CapturedContent = std::result::Result<Vec<u8>, u64>;

#[cfg(any(windows, test))]
const SNAPSHOT_READ_SIZE: usize = 64 * 1024;

#[cfg(any(windows, test))]
trait SnapshotSource {
    fn check_intact(&self) -> Result<()>;
    fn read_at(&mut self, offset: u64, target: &mut [u8]) -> Result<usize>;
}

/// Read at most one byte beyond the persisted limit, including files that grow
/// after their metadata probe. The source must invalidate any read overlapping
/// a change; checking again after each read rejects simultaneous break/EOF.
#[cfg(any(windows, test))]
fn capture_bounded(source: &mut impl SnapshotSource, max_bytes: usize) -> Result<CapturedContent> {
    let mut bytes = Vec::with_capacity(max_bytes.min(SNAPSHOT_READ_SIZE));
    let mut chunk = [0u8; SNAPSHOT_READ_SIZE];
    loop {
        source.check_intact()?;
        let wanted = max_bytes
            .saturating_sub(bytes.len())
            .saturating_add(1)
            .min(chunk.len());
        let length = source.read_at(bytes.len() as u64, &mut chunk[..wanted])?;
        anyhow::ensure!(length <= wanted, "snapshot read exceeded its buffer");
        source.check_intact()?;
        if length == 0 {
            return Ok(Ok(bytes));
        }
        if length > max_bytes.saturating_sub(bytes.len()) {
            return Ok(Err(bytes.len().saturating_add(length) as u64));
        }
        bytes.extend_from_slice(&chunk[..length]);
    }
}

impl FileSnapshot {
    fn matches(&self, other: &Self) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if self.identity != other.identity
                || self.metadata.file_attributes() != other.metadata.file_attributes()
                || self.metadata.creation_time() != other.metadata.creation_time()
            {
                return false;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if self.metadata.dev() != other.metadata.dev()
                || self.metadata.ino() != other.metadata.ino()
            {
                return false;
            }
        }
        self.metadata.len() == other.metadata.len()
            && self.modified == other.modified
            && self.content_readable == other.content_readable
    }
}

/// `None` is an obsolete job. A denied data read may still yield an independently
/// verified metadata snapshot for Reconcile; all unknown IO errors remain failures.
fn probe_job_file(job: &JobSpec) -> Result<Option<FileSnapshot>> {
    let (file, content_readable) = match open_job_path(job, || fs::File::open(&job.path))? {
        Ok(file) => (Some(file), true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error)
            if job.operation == JobOperation::Reconcile
                && allows_metadata_only_omission(&error) =>
        {
            return probe_metadata_file(job, false);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot open file: {}", job.path.display()))
                .context(DeferredFileFailure);
        }
    };
    probe_open_file(job, file, content_readable)
}

fn probe_metadata_file(job: &JobSpec, content_readable: bool) -> Result<Option<FileSnapshot>> {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
        };
        match open_job_path(job, || {
            fs::OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES.0)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                .open(&job.path)
        })? {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| {
                        format!("cannot verify metadata identity: {}", job.path.display())
                    })
                    .context(DeferredFileFailure);
            }
        }
    };
    #[cfg(not(windows))]
    let file = None;
    probe_open_file(job, file, content_readable)
}

fn probe_open_file(
    job: &JobSpec,
    file: Option<fs::File>,
    content_readable: bool,
) -> Result<Option<FileSnapshot>> {
    let metadata = file
        .as_ref()
        .map_or_else(|| fs::metadata(&job.path), fs::File::metadata)
        .with_context(|| format!("cannot read metadata: {}", job.path.display()))
        .context(DeferredFileFailure)?;
    if !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(windows)]
    let identity = {
        let file = file.as_ref().context("metadata handle missing")?;
        let identity = file_identity(file).context(DeferredFileFailure)?;
        if identity.reference_number != job.file_id {
            return Ok(None);
        }
        if job.operation == JobOperation::Reconcile
            && let Some(expected_root) = job.path.to_str().and_then(volume_guid_root)
        {
            // A GUID-anchored pathname can still cross volumes through a
            // junction. FRNs are only unique within their actual volume.
            let resolved = final_guid_path(file).context(DeferredFileFailure)?;
            if !volume_guid_root(&resolved)
                .is_some_and(|actual_root| actual_root.eq_ignore_ascii_case(expected_root))
            {
                return Ok(None);
            }
        }
        identity
    };
    let modified = metadata
        .modified()
        .with_context(|| format!("cannot read modification time: {}", job.path.display()))
        .context(DeferredFileFailure)?;
    Ok(Some(FileSnapshot {
        file,
        captured: None,
        metadata,
        modified,
        content_readable,
        #[cfg(windows)]
        identity,
    }))
}

/// Keep ordinary open errors available to the metadata-only policy, but do not
/// confuse a disappeared GUID volume with an obsolete file. Only a missing path
/// requires the extra root probe and one bounded re-open of that same path.
fn open_job_path(
    job: &JobSpec,
    open: impl Fn() -> std::io::Result<fs::File>,
) -> Result<std::io::Result<fs::File>> {
    let first = open();
    #[cfg(windows)]
    if job.operation == JobOperation::Reconcile
        && let Some(root) = job.path.to_str().and_then(volume_guid_root)
    {
        return retry_missing_with_verified_root(first, || verify_volume_root(root), open);
    }
    #[cfg(not(windows))]
    let _ = job;
    Ok(first)
}

#[cfg(any(windows, test))]
fn retry_missing_with_verified_root<T, Guard>(
    first: std::io::Result<T>,
    verify_root: impl FnOnce() -> Result<Guard>,
    reopen: impl FnOnce() -> std::io::Result<T>,
) -> Result<std::io::Result<T>> {
    if !first
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(first);
    }
    let _root = verify_root().context(DeferredFileFailure)?;
    // The volume may have returned between the first failure and this probe.
    // Hold its verified root handle while re-opening the original file once.
    Ok(reopen())
}

#[cfg(windows)]
fn verify_volume_root(root: &str) -> Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};

    // The trailing slash selects the directory, not a raw volume handle; this
    // asks only for attributes and does not require raw-disk privileges.
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(root)
        .with_context(|| format!("cannot verify volume root: {root}"))?;
    anyhow::ensure!(
        file.metadata()?.is_dir(),
        "volume root is not a directory: {root}"
    );
    let resolved = final_guid_path(&file)?;
    anyhow::ensure!(
        volume_guid_root(&resolved).is_some_and(|actual| actual.eq_ignore_ascii_case(root)),
        "volume root identity changed: {root}"
    );
    Ok(file)
}

fn allows_metadata_only_omission(error: &std::io::Error) -> bool {
    if error.kind() != std::io::ErrorKind::PermissionDenied {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};
        // A sharing/byte-range lock can disappear without a security or content
        // journal event. Keep its retry obligation even if a Rust version maps
        // the Win32 code to PermissionDenied and attribute-only access works.
        if [ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION]
            .iter()
            .any(|code| error.raw_os_error() == Some(code.0 as i32))
        {
            return false;
        }
    }
    true
}

/// Capture without denying application writers. Atomic Read-Handle oplocks
/// notify us when data changes and let us close before a conflicting open fails
/// its sharing check. CPU extraction runs only after all these handles close.
#[cfg(windows)]
mod native_snapshot {
    use super::*;
    use std::cell::UnsafeCell;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::{
        ERROR_HANDLE_EOF, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, GENERIC_READ, HANDLE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::Storage::FileSystem::{
        CREATEFILE2_EXTENDED_PARAMETERS, CreateFile2, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    };
    use windows::Win32::System::IO::{
        CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0,
    };
    use windows::Win32::System::Ioctl::{
        FSCTL_REQUEST_OPLOCK, OPLOCK_LEVEL_CACHE_HANDLE, OPLOCK_LEVEL_CACHE_READ,
        REQUEST_OPLOCK_CURRENT_VERSION, REQUEST_OPLOCK_INPUT_BUFFER,
        REQUEST_OPLOCK_INPUT_FLAG_REQUEST, REQUEST_OPLOCK_OUTPUT_BUFFER,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, INFINITE, ResetEvent, WaitForMultipleObjects, WaitForSingleObject,
    };
    use windows::Win32::System::WindowsProgramming::FILE_FLAG_OPEN_REQUIRING_OPLOCK;
    use windows::core::PCWSTR;

    const READ_WAIT_MS: u32 = 5_000;

    pub(super) fn win32_io_error(error: windows::core::Error) -> std::io::Error {
        let code = error.code().0 as u32;
        if code & 0xffff_0000 == 0x8007_0000 {
            std::io::Error::from_raw_os_error((code & 0xffff) as i32)
        } else {
            std::io::Error::other(error)
        }
    }

    fn open_atomic(job: &JobSpec) -> std::io::Result<fs::File> {
        let path: Vec<_> = job.path.as_os_str().encode_wide().chain(Some(0)).collect();
        let parameters = CREATEFILE2_EXTENDED_PARAMETERS {
            dwSize: std::mem::size_of::<CREATEFILE2_EXTENDED_PARAMETERS>() as u32,
            dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
            dwFileFlags: FILE_FLAG_OVERLAPPED.0 | FILE_FLAG_OPEN_REQUIRING_OPLOCK,
            ..Default::default()
        };
        // SAFETY: path is terminated and parameters remain alive for this call.
        // The returned handle is owned by File and used only with overlapped IO.
        let handle = unsafe {
            CreateFile2(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                OPEN_EXISTING,
                Some(&parameters),
            )
        }
        .map_err(win32_io_error)?;
        Ok(unsafe { fs::File::from_raw_handle(handle.0 as _) })
    }

    fn event() -> Result<OwnedHandle> {
        // SAFETY: unnamed, initially nonsignaled manual-reset event. The owned
        // handle outlives every OVERLAPPED request that refers to it.
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        Ok(unsafe { OwnedHandle::from_raw_handle(handle.0 as _) })
    }

    fn raw(handle: &impl AsRawHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle() as _)
    }

    struct OplockRequest {
        input: REQUEST_OPLOCK_INPUT_BUFFER,
        output: REQUEST_OPLOCK_OUTPUT_BUFFER,
        overlapped: OVERLAPPED,
    }

    struct ReadRequest {
        bytes: [u8; SNAPSHOT_READ_SIZE],
        overlapped: OVERLAPPED,
    }

    pub(super) struct Reader {
        file: Option<fs::File>,
        // Boxed storage never moves while the kernel owns these pointers.
        // UnsafeCell permits kernel writes while check_intact borrows Reader.
        oplock: Box<UnsafeCell<OplockRequest>>,
        read: Box<UnsafeCell<ReadRequest>>,
        oplock_event: OwnedHandle,
        read_event: OwnedHandle,
        oplock_pending: bool,
        read_pending: bool,
    }

    impl Reader {
        pub(super) fn open(job: &JobSpec) -> Result<Self> {
            let oplock_event = event()?;
            let read_event = event()?;
            let mut reader = Self {
                file: None,
                oplock: Box::new(UnsafeCell::new(OplockRequest {
                    input: REQUEST_OPLOCK_INPUT_BUFFER {
                        StructureVersion: REQUEST_OPLOCK_CURRENT_VERSION as u16,
                        StructureLength: std::mem::size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u16,
                        RequestedOplockLevel: OPLOCK_LEVEL_CACHE_READ | OPLOCK_LEVEL_CACHE_HANDLE,
                        Flags: REQUEST_OPLOCK_INPUT_FLAG_REQUEST,
                    },
                    output: REQUEST_OPLOCK_OUTPUT_BUFFER::default(),
                    overlapped: OVERLAPPED {
                        hEvent: raw(&oplock_event),
                        ..Default::default()
                    },
                })),
                read: Box::new(UnsafeCell::new(ReadRequest {
                    bytes: [0; SNAPSHOT_READ_SIZE],
                    overlapped: OVERLAPPED::default(),
                })),
                oplock_event,
                read_event,
                oplock_pending: false,
                read_pending: false,
            };
            // No filesystem operation on a successful atomic handle is allowed
            // between this open and the oplock request, including metadata.
            reader.file = Some(open_job_path(job, || open_atomic(job))??);
            let handle = raw(reader.file.as_ref().expect("opened snapshot handle"));
            // No request is pending while these buffers are initialized.
            let request = unsafe { &mut *reader.oplock.get() };
            // SAFETY: every retained input/output/OVERLAPPED pointer addresses
            // boxed storage owned until completion; no stack byte-count pointer
            // escapes. Drop closes the handle and drains both owned events.
            let result = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_REQUEST_OPLOCK,
                    Some(std::ptr::from_ref(&request.input).cast()),
                    std::mem::size_of::<REQUEST_OPLOCK_INPUT_BUFFER>() as u32,
                    Some(std::ptr::from_mut(&mut request.output).cast()),
                    std::mem::size_of::<REQUEST_OPLOCK_OUTPUT_BUFFER>() as u32,
                    None,
                    Some(&mut request.overlapped),
                )
            };
            match result {
                Err(error)
                    if win32_io_error(error.clone()).raw_os_error()
                        == Some(ERROR_IO_PENDING.0 as i32) =>
                {
                    reader.oplock_pending = true;
                    reader.check_intact()?;
                    Ok(reader)
                }
                Err(error) => Err(error).context("cannot establish snapshot oplock"),
                Ok(()) => anyhow::bail!("snapshot oplock completed without a pending lease"),
            }
        }

        fn metadata(&self, job: &JobSpec) -> Result<Option<FileSnapshot>> {
            self.check_intact()?;
            // DuplicateHandle preserves the same file object/oplock key. It
            // does not reopen a pathname that could block behind our own RH.
            let file = self
                .file
                .as_ref()
                .context("snapshot handle closed")?
                .try_clone()?;
            let mut snapshot = probe_open_file(job, Some(file), true)?;
            if let Some(snapshot) = &mut snapshot {
                snapshot.file = None;
            }
            self.check_intact()?;
            Ok(snapshot)
        }
    }

    impl SnapshotSource for Reader {
        fn check_intact(&self) -> Result<()> {
            // SAFETY: the event is owned and is never closed while self exists.
            match unsafe { WaitForSingleObject(raw(&self.oplock_event), 0) } {
                WAIT_TIMEOUT => Ok(()),
                WAIT_OBJECT_0 => anyhow::bail!("snapshot oplock broke; retry required"),
                _ => Err(std::io::Error::last_os_error()).context("cannot inspect snapshot oplock"),
            }
        }

        fn read_at(&mut self, offset: u64, target: &mut [u8]) -> Result<usize> {
            self.check_intact()?;
            anyhow::ensure!(
                !self.read_pending && target.len() <= SNAPSHOT_READ_SIZE,
                "invalid snapshot read state"
            );
            let handle = raw(self.file.as_ref().context("snapshot handle closed")?);
            // SAFETY: no prior read owns this event/request now.
            unsafe { ResetEvent(raw(&self.read_event)) }?;
            // No kernel read owns this cell until ReadFile is issued below.
            let request = unsafe { &mut *self.read.get() };
            request.overlapped = OVERLAPPED {
                hEvent: raw(&self.read_event),
                Anonymous: OVERLAPPED_0 {
                    Anonymous: OVERLAPPED_0_0 {
                        Offset: offset as u32,
                        OffsetHigh: (offset >> 32) as u32,
                    },
                },
                ..Default::default()
            };
            // SAFETY: the boxed buffer and OVERLAPPED remain alive, at the same
            // addresses, until completion or Drop's cancellation/close/drain.
            let result = unsafe {
                ReadFile(
                    handle,
                    Some(&mut request.bytes[..target.len()]),
                    None,
                    Some(&mut request.overlapped),
                )
            };
            match result {
                Ok(()) => {}
                Err(error) => match win32_io_error(error.clone()).raw_os_error() {
                    Some(code) if code == ERROR_HANDLE_EOF.0 as i32 => {
                        self.check_intact()?;
                        return Ok(0);
                    }
                    Some(code) if code == ERROR_IO_PENDING.0 as i32 => {
                        self.read_pending = true;
                        // Put break first so simultaneous read/break readiness
                        // invalidates the snapshot. Writes need no RH ACK; a
                        // conflicting open proceeds as soon as Drop closes us.
                        let ready = unsafe {
                            WaitForMultipleObjects(
                                &[raw(&self.oplock_event), raw(&self.read_event)],
                                false,
                                READ_WAIT_MS,
                            )
                        };
                        if ready == WAIT_OBJECT_0 {
                            anyhow::bail!("snapshot oplock broke during read; retry required");
                        }
                        anyhow::ensure!(
                            ready.0 == WAIT_OBJECT_0.0 + 1,
                            "snapshot read timed out or wait failed"
                        );
                    }
                    _ => return Err(error).context("snapshot read failed"),
                },
            }
            let mut length = 0;
            // SAFETY: immediate success or the read event establishes request
            // completion. The original file handle remains valid here.
            let completed = unsafe {
                GetOverlappedResult(
                    handle,
                    std::ptr::addr_of!((*self.read.get()).overlapped),
                    &mut length,
                    false,
                )
            };
            if completed.as_ref().is_err_and(|error| {
                win32_io_error(error.clone()).raw_os_error() == Some(ERROR_IO_INCOMPLETE.0 as i32)
            }) {
                self.read_pending = true;
                anyhow::bail!("snapshot read signaled without completing");
            }
            self.read_pending = false;
            if let Err(error) = completed {
                if win32_io_error(error.clone()).raw_os_error() == Some(ERROR_HANDLE_EOF.0 as i32) {
                    // An overlapped EOF can complete asynchronously as well as
                    // return directly from ReadFile. Both are terminal reads.
                    self.check_intact()?;
                    return Ok(0);
                }
                return Err(error).context("cannot finish snapshot read");
            }
            self.check_intact()?;
            let length = length as usize;
            anyhow::ensure!(length <= target.len(), "snapshot read exceeded its buffer");
            // Completion above ended the kernel's mutable access to this cell.
            let bytes = unsafe { &(*self.read.get()).bytes };
            target[..length].copy_from_slice(&bytes[..length]);
            Ok(length)
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            if let Some(file) = self.file.take() {
                // Cancel only a data read. Canceling the oplock before closing
                // its data handle could expose a sharing violation to a new
                // application opener in between those two operations.
                if self.read_pending {
                    // SAFETY: the pending request's stable allocation remains
                    // owned until its event is drained below, even on a race
                    // where cancellation reports ERROR_NOT_FOUND.
                    unsafe {
                        let _ = CancelIoEx(
                            raw(&file),
                            Some(std::ptr::addr_of!((*self.read.get()).overlapped)),
                        );
                    }
                }
                // Close releases the data handle and cancels/acknowledges RH
                // together. Do this before waiting for any outstanding read.
                drop(file);
            }
            for (pending, event) in [
                (self.read_pending, &self.read_event),
                (self.oplock_pending, &self.oplock_event),
            ] {
                if pending && unsafe { WaitForSingleObject(raw(event), INFINITE) } != WAIT_OBJECT_0
                {
                    // Continuing/unwinding could free buffers still owned by
                    // the kernel. A failed process cannot acknowledge its batch.
                    // Abort directly: even a logging subscriber could panic
                    // and unwind through these still-pending allocations.
                    std::process::abort();
                }
            }
        }
    }

    pub(super) fn capture(job: &JobSpec, max_bytes: usize) -> Result<Option<FileSnapshot>> {
        capture_inner(job, max_bytes).context(DeferredFileFailure)
    }

    fn capture_inner(job: &JobSpec, max_bytes: usize) -> Result<Option<FileSnapshot>> {
        let mut reader = match Reader::open(job) {
            Ok(reader) => reader,
            Err(error) => {
                if let Some(io) = error.downcast_ref::<std::io::Error>() {
                    if io.kind() == std::io::ErrorKind::NotFound
                        && !error.is::<DeferredFileFailure>()
                    {
                        return Ok(None);
                    }
                    if allows_metadata_only_omission(io) && !error.is::<DeferredFileFailure>() {
                        return probe_metadata_file(job, false);
                    }
                }
                return Err(error);
            }
        };
        let Some(before) = reader.metadata(job)? else {
            return Ok(None);
        };
        let bytes = if before.metadata.len() > max_bytes as u64 {
            Err(before.metadata.len())
        } else {
            capture_bounded(&mut reader, max_bytes)?
        };
        let Some(mut after) = reader.metadata(job)? else {
            return Ok(None);
        };
        anyhow::ensure!(
            before.matches(&after),
            "file metadata changed during snapshot capture"
        );
        // This includes empty/oversized files and breaks racing the last read
        // or metadata query. No data from a broken lease can become searchable.
        reader.check_intact()?;
        after.captured = Some(bytes);
        drop(reader);
        Ok(Some(after))
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowsFileIdentity {
    reference_number: u64,
    volume_serial: u32,
}

#[cfg(windows)]
fn file_identity(file: &fs::File) -> Result<WindowsFileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is borrowed from a live File and the output pointer
    // addresses a correctly sized, writable structure for this synchronous call.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle() as _), &mut info) }?;
    Ok(WindowsFileIdentity {
        reference_number: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        volume_serial: info.dwVolumeSerialNumber,
    })
}

#[cfg(any(windows, test))]
fn volume_guid_root(path: &str) -> Option<&str> {
    const PREFIX: &str = r"\\?\Volume{";
    let root = path.get(..49)?;
    if !root.get(..PREFIX.len())?.eq_ignore_ascii_case(PREFIX) || root.get(47..)? != "}\\" {
        return None;
    }
    let guid = root.get(PREFIX.len()..47)?;
    let valid = guid.bytes().enumerate().all(|(index, byte)| {
        if matches!(index, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    });
    valid.then_some(root)
}

#[cfg(windows)]
fn final_guid_path(file: &fs::File) -> Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_GUID};

    let mut buffer = vec![0u16; 1024];
    for _ in 0..2 {
        // SAFETY: the live File owns the handle; Windows writes at most the
        // supplied buffer length during this synchronous call.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle() as _),
                &mut buffer,
                VOLUME_NAME_GUID,
            )
        } as usize;
        if length == 0 {
            return Err(windows::core::Error::from_win32())
                .context("cannot resolve verified file volume");
        }
        if length >= buffer.len() {
            anyhow::ensure!(length < 32768, "resolved file path exceeds Windows limit");
            buffer.resize(length + 1, 0);
            continue;
        }
        return String::from_utf16(&buffer[..length]).context("resolved file path is not Unicode");
    }
    anyhow::bail!("file path changed repeatedly during volume verification")
}

fn to_content_doc(
    job: &JobSpec,
    meta: &std::fs::Metadata,
    out: content_extractor::ExtractedContent,
) -> Result<content_index::ContentDoc> {
    let created = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();

    let name = job
        .path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());

    let ext = job
        .path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());

    Ok(content_index::ContentDoc {
        key: out.key,
        volume: job.volume_id,
        name,
        path: job.path.to_str().map(|s| s.to_string()),
        ext,
        size: meta.len(),
        created,
        modified,
        flags: u64::from(file_flags(meta).bits()),
        content_lang: out.content_lang.clone(),
        content: out.text,
    })
}

fn file_flags(meta: &fs::Metadata) -> core_types::FileFlags {
    use core_types::FileFlags;
    let mut flags = FileFlags::empty();
    flags.set(FileFlags::IS_DIR, meta.is_dir());
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_OFFLINE,
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY,
        };
        let attributes = meta.file_attributes();
        for (mask, flag) in [
            (FILE_ATTRIBUTE_ARCHIVE, FileFlags::ARCHIVE),
            (FILE_ATTRIBUTE_HIDDEN, FileFlags::HIDDEN),
            (FILE_ATTRIBUTE_OFFLINE, FileFlags::OFFLINE),
            (FILE_ATTRIBUTE_REPARSE_POINT, FileFlags::REPARSE),
            (FILE_ATTRIBUTE_SYSTEM, FileFlags::SYSTEM),
            (FILE_ATTRIBUTE_TEMPORARY, FileFlags::TEMPORARY),
        ] {
            flags.set(flag, attributes & mask.0 != 0);
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use content_extractor::Extractor;
    use tantivy::Term;
    use tantivy::collector::Count;
    use tantivy::query::TermQuery;
    use tantivy::schema::IndexRecordOption;

    fn args() -> Args {
        Args {
            volume_id: None,
            file_id: None,
            path: None,
            index_dir: PathBuf::new(),
            max_bytes: 1024,
            max_chars: 1024,
            enable_extractous: false,
            json: false,
            preview_chars: 0,
            job_file: None,
            commit_every: 0,
        }
    }

    fn job_for(path: PathBuf) -> Result<JobSpec> {
        #[cfg(windows)]
        let file_id = file_identity(&fs::File::open(&path)?)?.reference_number;
        #[cfg(not(windows))]
        let file_id = 0xfedc_0000_0000_002a;
        Ok(JobSpec {
            volume_id: 7,
            file_id,
            path,
            operation: JobOperation::Reconcile,
            max_bytes: None,
            max_chars: None,
            file_size: 0,
        })
    }

    fn content_matches(index: &ContentIndex, text: &str) -> Result<usize> {
        let query = TermQuery::new(
            Term::from_field_text(index.fields.content, text),
            IndexRecordOption::Basic,
        );
        let reader = content_index::open_reader(index)?;
        Ok(reader.searcher().search(&query, &Count)?)
    }

    fn durable_batch(jobs: Vec<JobSpec>) -> JobFile {
        JobFile {
            version: 3,
            id: Some(Uuid::new_v4()),
            reset_volumes: Vec::new(),
            jobs,
        }
    }

    fn deferrable_batch(jobs: Vec<JobSpec>) -> JobFile {
        JobFile {
            version: 4,
            ..durable_batch(jobs)
        }
    }

    struct SnapshotFixture {
        data: std::io::Cursor<Vec<u8>>,
        max_read: usize,
        reads: usize,
        break_on_read: Option<usize>,
        fail_on_read: Option<usize>,
    }

    impl SnapshotFixture {
        fn new(data: &[u8], max_read: usize) -> Self {
            Self {
                data: std::io::Cursor::new(data.to_vec()),
                max_read,
                reads: 0,
                break_on_read: None,
                fail_on_read: None,
            }
        }
    }

    impl SnapshotSource for SnapshotFixture {
        fn check_intact(&self) -> Result<()> {
            anyhow::ensure!(
                self.break_on_read != Some(self.reads),
                "capture invalidated"
            );
            Ok(())
        }
        fn read_at(&mut self, offset: u64, target: &mut [u8]) -> Result<usize> {
            use std::io::Read;
            self.reads += 1;
            if self.fail_on_read == Some(self.reads) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
            }
            self.data.set_position(offset);
            let length = target.len().min(self.max_read);
            Ok(self.data.read(&mut target[..length])?)
        }
    }

    #[test]
    fn bounded_snapshot_enforces_limits_across_short_reads() -> Result<()> {
        let mut source = SnapshotFixture::new(b"0123456789", 3);
        assert_eq!(capture_bounded(&mut source, 8)?, Err(9));
        assert_eq!(source.data.position(), 9);
        assert_eq!(source.reads, 3);

        let mut source = SnapshotFixture::new(b"0123456789", 3);
        assert_eq!(
            capture_bounded(&mut source, 10)?,
            Ok(b"0123456789".to_vec())
        );
        assert_eq!(source.data.position(), 10);
        let mut empty = SnapshotFixture::new(b"", 3);
        assert_eq!(capture_bounded(&mut empty, 0)?, Ok(Vec::new()));
        let mut nonempty = SnapshotFixture::new(b"more", 3);
        assert_eq!(capture_bounded(&mut nonempty, 0)?, Err(1));
        assert_eq!(nonempty.data.position(), 1);
        Ok(())
    }

    #[test]
    fn bounded_snapshot_rejects_breaks_before_read_after_read_and_at_eof() {
        for break_on_read in [0, 1, 2] {
            let mut source = SnapshotFixture::new(b"content", 7);
            source.break_on_read = Some(break_on_read);
            let error = capture_bounded(&mut source, 7).unwrap_err();
            assert!(error.to_string().contains("capture invalidated"));
            assert_eq!(source.reads, break_on_read);
        }
        // A break completing with EOF or the size-limit probe must not be
        // accepted as a stable empty file or an intentional content omission.
        for bytes in [&b""[..], &b"x"[..]] {
            let mut source = SnapshotFixture::new(bytes, 1);
            source.break_on_read = Some(1);
            assert!(capture_bounded(&mut source, 0).is_err());
        }
    }

    #[test]
    fn bounded_snapshot_propagates_read_failure_without_omission() {
        let mut source = SnapshotFixture::new(b"content", 3);
        source.fail_on_read = Some(2);
        let error = capture_bounded(&mut source, 7).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Interrupted
        );
        assert_eq!(source.reads, 2);
        assert_eq!(source.data.position(), 3);
    }

    /// Inject a repeatable read failure for one actual file while letting the
    /// production verified-handle extractor read every other file normally.
    struct FailingFileExtractor {
        key: DocKey,
        remaining: std::sync::Mutex<usize>,
    }

    impl FailingFileExtractor {
        fn check(&self, key: DocKey) -> std::result::Result<(), content_extractor::ExtractError> {
            let mut remaining = self.remaining.lock().expect("fault counter lock");
            if key == self.key && *remaining > 0 {
                *remaining -= 1;
                return Err(content_extractor::ExtractError::Failed(
                    "file read is temporarily blocked".into(),
                ));
            }
            Ok(())
        }
    }

    impl content_extractor::Extractor for FailingFileExtractor {
        fn name(&self) -> &'static str {
            "deferred-file-regression"
        }
        fn supports(&self, _: &ExtractContext) -> bool {
            true
        }
        fn extract(
            &self,
            ctx: &ExtractContext,
            key: DocKey,
        ) -> std::result::Result<content_extractor::ExtractedContent, content_extractor::ExtractError>
        {
            self.check(key)?;
            content_extractor::SimpleTextExtractor.extract(ctx, key)
        }
        fn extract_file(
            &self,
            ctx: &ExtractContext,
            key: DocKey,
            file: &fs::File,
        ) -> std::result::Result<content_extractor::ExtractedContent, content_extractor::ExtractError>
        {
            self.check(key)?;
            content_extractor::SimpleTextExtractor.extract_file(ctx, key, file)
        }
        fn extract_bytes(
            &self,
            ctx: &ExtractContext,
            key: DocKey,
            bytes: &[u8],
        ) -> std::result::Result<content_extractor::ExtractedContent, content_extractor::ExtractError>
        {
            self.check(key)?;
            content_extractor::SimpleTextExtractor.extract_bytes(ctx, key, bytes)
        }
    }

    fn failing_file_stack(key: DocKey, failures: usize) -> ExtractorStack {
        ExtractorStack::new(vec![Box::new(FailingFileExtractor {
            key,
            remaining: std::sync::Mutex::new(failures),
        })])
    }

    #[test]
    fn deferred_file_allows_other_replacements_and_recovers_without_stale_replay() -> Result<()> {
        for commit_every in [0, 1] {
            let dir = tempfile::tempdir()?;
            let blocked_path = dir.path().join("blocked.txt");
            let available_path = dir.path().join("available.txt");
            let deleted_path = dir.path().join("deleted.txt");
            fs::write(&blocked_path, "blockedobsolete")?;
            fs::write(&available_path, "availableobsolete")?;
            fs::write(&deleted_path, "deletedobsolete")?;
            let blocked_job = job_for(blocked_path.clone())?;
            let available_job = job_for(available_path.clone())?;
            let deleted_job = job_for(deleted_path)?;
            #[cfg(not(windows))]
            let (available_job, mut deleted_job) = {
                let mut available_job = available_job;
                let mut deleted_job = deleted_job;
                available_job.file_id += 1;
                deleted_job.file_id += 2;
                (available_job, deleted_job)
            };
            #[cfg(windows)]
            let mut deleted_job = deleted_job;
            let blocked_key = DocKey::from_parts(blocked_job.volume_id, blocked_job.file_id);
            let index_path = dir.path().join("content");
            let index = content_index::open_or_create(&index_path)?;
            let config = WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            };
            let mut writer = content_index::create_writer(&index, &config)?;
            let stack = ExtractorStack::with_defaults();
            let mut args = args();
            args.commit_every = commit_every;
            process_batch(
                &stack,
                &index,
                &mut writer,
                durable_batch(vec![
                    blocked_job.clone(),
                    available_job.clone(),
                    deleted_job.clone(),
                ]),
                &args,
            )?;
            fs::write(&blocked_path, "blockedreplacement")?;
            fs::write(&available_path, "availablereplacement")?;
            deleted_job.operation = JobOperation::Delete;
            deleted_job.path = PathBuf::new();
            let pending = deferrable_batch(vec![blocked_job.clone(), available_job, deleted_job]);
            let failing = failing_file_stack(blocked_key, usize::MAX);
            process_batch(&failing, &index, &mut writer, pending.clone(), &args)?;
            let outcome = content_index::batch_outcome(&index.index)?.unwrap();
            assert!(outcome.receipt.complete);
            assert_eq!(Some(outcome.receipt.batch_id), pending.id);
            assert_eq!(outcome.deferred, vec![blocked_key]);
            for obsolete in ["blockedobsolete", "availableobsolete", "deletedobsolete"] {
                assert_eq!(content_matches(&index, obsolete)?, 0);
            }
            assert_eq!(content_matches(&index, "blockedreplacement")?, 0);
            assert_eq!(content_matches(&index, "availablereplacement")?, 1);
            drop(writer);
            drop(index);

            // A restart sees the same tombstone and obligation atomically.
            let index = content_index::open_or_create(&index_path)?;
            assert_eq!(
                content_index::batch_outcome(&index.index)?,
                Some(outcome.clone())
            );
            let mut writer = content_index::create_writer(&index, &config)?;
            process_batch(&failing, &index, &mut writer, pending, &args)?;
            let replayed = content_index::batch_outcome(&index.index)?.unwrap();
            assert_eq!(replayed.deferred, vec![blocked_key]);
            assert_ne!(replayed.receipt.commit_id, outcome.receipt.commit_id);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 1);

            let retry = deferrable_batch(vec![blocked_job.clone()]);
            process_batch(&stack, &index, &mut writer, retry.clone(), &args)?;
            assert!(
                content_index::batch_outcome(&index.index)?
                    .unwrap()
                    .deferred
                    .is_empty()
            );
            process_batch(&stack, &index, &mut writer, retry, &args)?;
            assert_eq!(content_matches(&index, "blockedreplacement")?, 1);
            assert_eq!(content_matches(&index, "blockedobsolete")?, 0);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 2);
            let mut deletion = blocked_job;
            deletion.operation = JobOperation::Delete;
            deletion.path = PathBuf::new();
            let deleted = deferrable_batch(vec![deletion]);
            process_batch(&stack, &index, &mut writer, deleted.clone(), &args)?;
            process_batch(&stack, &index, &mut writer, deleted, &args)?;
            assert!(
                content_index::batch_outcome(&index.index)?
                    .unwrap()
                    .deferred
                    .is_empty()
            );
            assert_eq!(content_matches(&index, "blockedreplacement")?, 0);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 1);
        }
        Ok(())
    }

    #[test]
    fn later_success_or_delete_supersedes_deferral_within_the_same_batch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("superseded.txt");
        fs::write(&path, "supersededword")?;
        let job = job_for(path)?;
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let args = args();
        process_batch(
            &failing_file_stack(key, 1),
            &index,
            &mut writer,
            deferrable_batch(vec![job.clone(), job.clone()]),
            &args,
        )?;
        assert!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred
                .is_empty()
        );
        assert_eq!(content_matches(&index, "supersededword")?, 1);
        let mut deletion = job.clone();
        deletion.operation = JobOperation::Delete;
        deletion.path = PathBuf::new();
        process_batch(
            &failing_file_stack(key, 1),
            &index,
            &mut writer,
            deferrable_batch(vec![job, deletion]),
            &args,
        )?;
        assert!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred
                .is_empty()
        );
        assert_eq!(content_matches(&index, "supersededword")?, 0);
        Ok(())
    }

    #[test]
    fn validation_and_strict_job_failures_never_publish_a_deferred_complete() -> Result<()> {
        for commit_every in [0, 1] {
            let dir = tempfile::tempdir()?;
            let first_path = dir.path().join("first.txt");
            let strict_path = dir.path().join("strict.txt");
            fs::write(&first_path, "originalfirst")?;
            fs::write(&strict_path, "originalstrict")?;
            let first = job_for(first_path.clone())?;
            let mut strict = job_for(strict_path.clone())?;
            #[cfg(not(windows))]
            {
                strict.file_id += 1;
            }
            strict.operation = JobOperation::Upsert;
            let first_key = DocKey::from_parts(first.volume_id, first.file_id);
            let index = content_index::create_in_ram()?;
            let mut writer = content_index::create_writer(
                &index,
                &WriterConfig {
                    heap_size_bytes: 20_000_000,
                    num_threads: 1,
                },
            )?;
            let mut args = args();
            args.commit_every = commit_every;
            let batch = deferrable_batch(vec![first.clone(), strict]);
            process_batch(
                &ExtractorStack::with_defaults(),
                &index,
                &mut writer,
                batch.clone(),
                &args,
            )?;
            let completed = content_index::batch_outcome(&index.index)?.unwrap();
            fs::write(&first_path, "replacementfirst")?;
            let mut invalid = first;
            invalid.path = PathBuf::new();
            let invalid_batch = deferrable_batch(vec![invalid]);
            let error = process_batch(
                &ExtractorStack::with_defaults(),
                &index,
                &mut writer,
                invalid_batch,
                &args,
            )
            .unwrap_err();
            assert!(error.to_string().contains("path must not be empty"));
            assert!(!error.is::<DeferredFileFailure>());
            assert_eq!(
                content_index::batch_outcome(&index.index)?,
                Some(completed.clone())
            );
            assert_eq!(content_matches(&index, "originalfirst")?, 1);

            // A deferrable read plus a strict job failure revokes completion
            // from an earlier successful attempt of this very same batch.
            fs::rename(&strict_path, dir.path().join("retained.txt"))?;
            let error = process_batch(
                &failing_file_stack(first_key, 1),
                &index,
                &mut writer,
                batch,
                &args,
            )
            .unwrap_err();
            assert!(error.to_string().contains("1 failed job"));
            let partial = content_index::batch_outcome(&index.index)?.unwrap();
            assert!(!partial.receipt.complete);
            assert!(partial.deferred.is_empty());
            assert_eq!(partial.receipt.batch_id, completed.receipt.batch_id);
            assert_ne!(partial.receipt.commit_id, completed.receipt.commit_id);
            assert_eq!(content_index::committed_batch(&index.index)?, None);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);
        }
        Ok(())
    }

    #[test]
    fn changed_snapshot_is_deferred_and_retry_indexes_only_the_current_content() -> Result<()> {
        struct AppendingExtractor;
        impl Extractor for AppendingExtractor {
            fn name(&self) -> &'static str {
                "snapshot-drift-regression"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                let file = fs::File::open(ctx.path)
                    .map_err(|error| content_extractor::ExtractError::Failed(error.to_string()))?;
                self.extract_file(ctx, key, &file)
            }
            fn extract_file(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                file: &fs::File,
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                use std::io::Write;
                let extracted =
                    content_extractor::SimpleTextExtractor.extract_file(ctx, key, file)?;
                fs::OpenOptions::new()
                    .append(true)
                    .open(ctx.path)
                    .and_then(|mut writer| writer.write_all(b" appendedafterread"))
                    .map_err(|error| content_extractor::ExtractError::Failed(error.to_string()))?;
                Ok(extracted)
            }
            fn extract_bytes(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                bytes: &[u8],
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                use std::io::Write;
                let extracted =
                    content_extractor::SimpleTextExtractor.extract_bytes(ctx, key, bytes)?;
                // Windows must have released its capture handle before CPU
                // extraction; application writes remain free to proceed.
                fs::OpenOptions::new()
                    .append(true)
                    .open(ctx.path)
                    .and_then(|mut writer| writer.write_all(b" appendedafterread"))
                    .map_err(|error| content_extractor::ExtractError::Failed(error.to_string()))?;
                Ok(extracted)
            }
        }
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("changing.txt");
        fs::write(&path, "stableoriginal")?;
        let job = job_for(path.clone())?;
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let args = args();
        let normal = ExtractorStack::with_defaults();
        process_batch(
            &normal,
            &index,
            &mut writer,
            durable_batch(vec![job.clone()]),
            &args,
        )?;
        fs::write(&path, "currentword")?;
        let pending = deferrable_batch(vec![job.clone()]);
        process_batch(
            &ExtractorStack::new(vec![Box::new(AppendingExtractor)]),
            &index,
            &mut writer,
            pending,
            &args,
        )?;
        let outcome = content_index::batch_outcome(&index.index)?.unwrap();
        assert!(outcome.receipt.complete);
        assert_eq!(outcome.deferred, vec![key]);
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);
        process_batch(
            &normal,
            &index,
            &mut writer,
            deferrable_batch(vec![job]),
            &args,
        )?;
        assert!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred
                .is_empty()
        );
        assert_eq!(content_matches(&index, "stableoriginal")?, 0);
        assert_eq!(content_matches(&index, "currentword")?, 1);
        assert_eq!(content_matches(&index, "appendedafterread")?, 1);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_sharing_violation_defers_one_file_and_recovers_after_unlock() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir()?;
        let blocked_path = dir.path().join("locked.txt");
        let available_path = dir.path().join("available.txt");
        fs::write(&blocked_path, "lockedobsolete")?;
        fs::write(&available_path, "availableword")?;
        let blocked = job_for(blocked_path.clone())?;
        let available = job_for(available_path)?;
        let key = DocKey::from_parts(blocked.volume_id, blocked.file_id);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let args = args();
        let stack = ExtractorStack::with_defaults();
        process_batch(
            &stack,
            &index,
            &mut writer,
            durable_batch(vec![blocked.clone()]),
            &args,
        )?;
        fs::write(&blocked_path, "lockedreplacement")?;
        let exclusive = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&blocked_path)?;
        let pending = deferrable_batch(vec![blocked, available]);
        process_batch(&stack, &index, &mut writer, pending.clone(), &args)?;
        let outcome = content_index::batch_outcome(&index.index)?.unwrap();
        assert_eq!(outcome.deferred, vec![key]);
        assert!(outcome.receipt.complete);
        assert_eq!(content_matches(&index, "lockedobsolete")?, 0);
        assert_eq!(content_matches(&index, "lockedreplacement")?, 0);
        assert_eq!(content_matches(&index, "availableword")?, 1);
        drop(exclusive);
        process_batch(&stack, &index, &mut writer, pending, &args)?;
        assert!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred
                .is_empty()
        );
        assert_eq!(content_matches(&index, "lockedreplacement")?, 1);
        assert_eq!(content_matches(&index, "availableword")?, 1);
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 2);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_oplock_discards_capture_overlapping_a_shared_writer() -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        struct ConcurrentWriter {
            reader: native_snapshot::Reader,
            writer: fs::File,
            wrote: bool,
        }

        impl SnapshotSource for ConcurrentWriter {
            fn check_intact(&self) -> Result<()> {
                self.reader.check_intact()
            }
            fn read_at(&mut self, offset: u64, target: &mut [u8]) -> Result<usize> {
                let length = self.reader.read_at(offset, target)?;
                if !self.wrote {
                    // Same-length overwrites cannot be detected from length,
                    // and Windows need not publish mtime until writers close.
                    self.writer.seek(SeekFrom::Start(0))?;
                    self.writer.write_all(b"newcontent")?;
                    self.wrote = true;
                }
                Ok(length)
            }
        }

        // Existing shared writer handles may coexist with RH. A new shared
        // writer is also allowed; actual data writes invalidate either lease.
        for writer_open_first in [true, false] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("changing.txt");
            fs::write(&path, b"oldcontent")?;
            let mut job = job_for(path.clone())?;
            job.path = PathBuf::from(final_guid_path(&fs::File::open(&path)?)?);
            let open_writer = || {
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
                    .open(&path)
            };
            let first_writer = if writer_open_first {
                Some(open_writer()?)
            } else {
                None
            };
            let reader = native_snapshot::Reader::open(&job)?;
            let writer = match first_writer {
                Some(writer) => writer,
                None => open_writer()?,
            };
            let mut changing = ConcurrentWriter {
                reader,
                writer,
                wrote: false,
            };
            let error = capture_bounded(&mut changing, 1024).unwrap_err();
            assert!(changing.wrote, "the application write must succeed");
            assert!(error.to_string().contains("oplock broke"));
            assert_eq!(
                changing.writer.metadata()?.len(),
                b"oldcontent".len() as u64
            );
            drop(changing);

            // No further data write is needed: release and retry captures the
            // complete current document through the production capture path.
            let recovered = native_snapshot::capture(&job, 1024)?.context("expected snapshot")?;
            assert_eq!(recovered.captured, Some(Ok(b"newcontent".to_vec())));
            assert_eq!(recovered.metadata.len(), b"newcontent".len() as u64);
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_new_exclusive_writer_succeeds_after_capture_acknowledges_break() -> Result<()> {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("exclusive.txt");
        fs::write(&path, b"oldcontent")?;
        let job = job_for(path.clone())?;
        let reader = native_snapshot::Reader::open(&job)?;
        let (sent, received) = mpsc::channel();
        let application = std::thread::spawn(move || {
            let result = (|| -> std::io::Result<()> {
                let mut writer = fs::OpenOptions::new()
                    .write(true)
                    .share_mode(0)
                    .open(path)?;
                writer.write_all(b"newcontent")
            })();
            let _ = sent.send(result);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut broke = false;
        while Instant::now() < deadline {
            if reader.check_intact().is_err() {
                broke = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // Conflicting opens need RH acknowledgement. Always close before any
        // join/receive/assertion so failure paths cannot strand the application.
        drop(reader);
        received.recv_timeout(Duration::from_secs(5))??;
        application
            .join()
            .map_err(|_| anyhow::anyhow!("application writer panicked"))?;
        assert!(broke, "exclusive writer must signal the RH break");
        let current = native_snapshot::capture(&job, 1024)?.context("expected snapshot")?;
        assert_eq!(current.captured, Some(Ok(b"newcontent".to_vec())));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_extraction_and_final_probe_allow_a_new_exclusive_writer() -> Result<()> {
        use std::os::windows::fs::OpenOptionsExt;
        use std::sync::{Arc, Mutex};

        struct ExclusiveDuringExtraction(Arc<Mutex<Option<fs::File>>>);
        impl Extractor for ExclusiveDuringExtraction {
            fn name(&self) -> &'static str {
                "exclusive-writer-during-extraction"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                _: &ExtractContext,
                _: DocKey,
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                Err(content_extractor::ExtractError::Failed(
                    "snapshot bytes required".into(),
                ))
            }
            fn extract_bytes(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                bytes: &[u8],
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                let writer = fs::OpenOptions::new()
                    .write(true)
                    .share_mode(0)
                    .open(ctx.path)
                    .map_err(|error| content_extractor::ExtractError::Failed(error.to_string()))?;
                *self.0.lock().expect("writer guard lock") = Some(writer);
                content_extractor::SimpleTextExtractor.extract_bytes(ctx, key, bytes)
            }
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("extraction.txt");
        fs::write(&path, b"verifiedcontent")?;
        let job = job_for(path)?;
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        let held = Arc::new(Mutex::new(None));
        let stack =
            ExtractorStack::new(vec![Box::new(ExclusiveDuringExtraction(Arc::clone(&held)))]);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        process_batch(
            &stack,
            &index,
            &mut writer,
            deferrable_batch(vec![job]),
            &args(),
        )?;
        assert!(held.lock().expect("writer guard lock").is_some());
        let outcome = content_index::batch_outcome(&index.index)?.unwrap();
        assert!(outcome.receipt.complete);
        assert!(outcome.deferred.is_empty());
        assert_eq!(content_matches(&index, "verifiedcontent")?, 1);
        assert!(
            content_index::read_file_meta(&index, &content_index::open_reader(&index)?, key)?
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn empty_and_reset_batches_commit_durable_receipts() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("reset.txt");
        fs::write(&path, "obsoleteword")?;
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::with_defaults();
        let mut args = args();
        let empty = durable_batch(Vec::new());
        process_batch(&stack, &index, &mut writer, empty.clone(), &args)?;
        assert_eq!(content_index::committed_batch(&index.index)?, empty.id);
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);

        let created = durable_batch(vec![job_for(path)?]);
        process_batch(&stack, &index, &mut writer, created.clone(), &args)?;
        assert_eq!(content_index::committed_batch(&index.index)?, created.id);
        assert_eq!(content_matches(&index, "obsoleteword")?, 1);

        let mut reset = durable_batch(Vec::new());
        reset.reset_volumes = vec![7];
        args.commit_every = 1;
        process_batch(&stack, &index, &mut writer, reset.clone(), &args)?;
        assert_eq!(content_index::committed_batch(&index.index)?, reset.id);
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);
        process_batch(&stack, &index, &mut writer, reset.clone(), &args)?;
        assert_eq!(content_index::committed_batch(&index.index)?, reset.id);

        // A later empty transaction must advance the receipt even when an
        // earlier reset already left the index without segments.
        let after_reset = durable_batch(Vec::new());
        process_batch(&stack, &index, &mut writer, after_reset.clone(), &args)?;
        assert_eq!(
            content_index::committed_batch(&index.index)?,
            after_reset.id
        );
        Ok(())
    }

    #[test]
    fn partial_batch_receipt_is_not_success_and_replay_replaces_documents() -> Result<()> {
        for commit_every in [0, 1] {
            let dir = tempfile::tempdir()?;
            let first_path = dir.path().join("first.txt");
            let retry_path = dir.path().join("retry.txt");
            let held_path = dir.path().join("retained.txt");
            fs::write(&first_path, "firstobsolete")?;
            fs::write(&retry_path, "retryobsolete")?;
            let first_job = job_for(first_path.clone())?;
            let mut retry_job = job_for(retry_path.clone())?;
            #[cfg(not(windows))]
            {
                retry_job.file_id = first_job.file_id + 1;
            }
            retry_job.operation = JobOperation::Upsert;
            let index = content_index::create_in_ram()?;
            let mut writer = content_index::create_writer(
                &index,
                &WriterConfig {
                    heap_size_bytes: 20_000_000,
                    num_threads: 1,
                },
            )?;
            let stack = ExtractorStack::with_defaults();
            let mut args = args();
            let original = durable_batch(vec![first_job.clone(), retry_job.clone()]);
            process_batch(&stack, &index, &mut writer, original, &args)?;
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 2);

            fs::write(&first_path, "firstreplacement")?;
            fs::rename(&retry_path, &held_path)?;
            let pending = durable_batch(vec![first_job, retry_job]);
            // Each partial commit must carry the pending identity. The worker must
            // still fail rather than promoting this receipt to an acknowledgement.
            args.commit_every = commit_every;
            let error =
                process_batch(&stack, &index, &mut writer, pending.clone(), &args).unwrap_err();
            assert!(error.to_string().contains("1 failed job"));
            assert_eq!(content_index::committed_batch(&index.index)?, None);
            let partial = content_index::batch_receipt(&index.index)?.unwrap();
            assert_eq!(Some(partial.batch_id), pending.id);
            assert!(!partial.complete);
            assert_eq!(content_matches(&index, "firstobsolete")?, 0);
            assert_eq!(content_matches(&index, "retryobsolete")?, 0);
            assert_eq!(content_matches(&index, "firstreplacement")?, 1);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 1);

            fs::rename(&held_path, &retry_path)?;
            fs::write(&retry_path, "retryreplacement")?;
            process_batch(&stack, &index, &mut writer, pending.clone(), &args)?;
            let completed = content_index::batch_receipt(&index.index)?.unwrap();
            assert!(completed.complete);
            assert_ne!(completed.commit_id, partial.commit_id);
            // The service can still have this batch pending after both indexes
            // completed. Failure during replay must revoke the old completion.
            fs::rename(&retry_path, &held_path)?;
            assert!(process_batch(&stack, &index, &mut writer, pending.clone(), &args).is_err());
            assert_eq!(content_index::committed_batch(&index.index)?, None);
            let retried_partial = content_index::batch_receipt(&index.index)?.unwrap();
            assert_eq!(retried_partial.batch_id, completed.batch_id);
            assert!(!retried_partial.complete);
            assert_ne!(retried_partial.commit_id, completed.commit_id);
            fs::rename(&held_path, &retry_path)?;
            process_batch(&stack, &index, &mut writer, pending.clone(), &args)?;
            assert_eq!(content_index::committed_batch(&index.index)?, pending.id);
            let replayed = content_index::batch_receipt(&index.index)?.unwrap();
            assert!(replayed.complete);
            assert_ne!(replayed.commit_id, completed.commit_id);
            assert_eq!(content_matches(&index, "firstreplacement")?, 1);
            assert_eq!(content_matches(&index, "retryreplacement")?, 1);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 2);

            let deletes = durable_batch(
                pending
                    .jobs
                    .into_iter()
                    .map(|mut job| {
                        job.operation = JobOperation::Delete;
                        job.path = PathBuf::new();
                        job
                    })
                    .collect(),
            );
            process_batch(&stack, &index, &mut writer, deletes.clone(), &args)?;
            assert_eq!(content_index::committed_batch(&index.index)?, deletes.id);
            assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);
        }
        Ok(())
    }

    #[test]
    fn worker_replay_modify_rename_and_delete_real_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let original = dir.path().join("original.txt");
        fs::write(&original, "obsoleteword")?;
        let mut job = job_for(original.clone())?;
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::with_defaults();
        let args = args();
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "obsoleteword")?, 1);

        fs::write(&original, "replacementword longer")?;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "obsoleteword")?, 0);
        assert_eq!(content_matches(&index, "replacementword")?, 1);

        let renamed = dir.path().join("renamed.txt");
        fs::rename(&original, &renamed)?;
        job.path = renamed;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "replacementword")?, 1);
        let reader = content_index::open_reader(&index)?;
        let old_name = TermQuery::new(
            Term::from_field_text(index.fields.name, "original"),
            IndexRecordOption::Basic,
        );
        assert_eq!(reader.searcher().search(&old_name, &Count)?, 0);
        assert_eq!(reader.searcher().num_docs(), 1);

        job.operation = JobOperation::Delete;
        job.path = PathBuf::new();
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        process_job(&stack, &index, &mut writer, job, &args)?;
        writer.commit()?;
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 0);
        Ok(())
    }

    #[test]
    fn missing_reconcile_removes_old_content_but_default_upsert_fails() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("original.txt");
        fs::write(&path, "obsoleteword")?;
        let mut job = job_for(path.clone())?;
        #[cfg(windows)]
        {
            job.path = PathBuf::from(final_guid_path(&fs::File::open(&path)?)?);
        }
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::with_defaults();
        let args = args();
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        fs::rename(path, dir.path().join("moved.txt"))?;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "obsoleteword")?, 0);
        job.operation = JobOperation::Upsert;
        let error = process_job(&stack, &index, &mut writer, job, &args).unwrap_err();
        assert!(format!("{error:#}").contains("file missing"));
        Ok(())
    }

    #[test]
    fn missing_file_verifies_volume_and_reopens_once_before_becoming_obsolete() -> Result<()> {
        use std::cell::Cell;
        use std::io::{Error, ErrorKind};

        let root_checks = Cell::new(0);
        let reopens = Cell::new(0);
        let unavailable = retry_missing_with_verified_root::<usize, ()>(
            Err(Error::from(ErrorKind::NotFound)),
            || {
                root_checks.set(root_checks.get() + 1);
                Err(Error::from(ErrorKind::NotFound).into())
            },
            || {
                reopens.set(reopens.get() + 1);
                Ok(17)
            },
        )
        .unwrap_err();
        assert!(unavailable.is::<DeferredFileFailure>());
        assert_eq!(root_checks.get(), 1);
        assert_eq!(reopens.get(), 0);

        struct RootLease<'a>(&'a Cell<bool>);
        impl Drop for RootLease<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let held = Cell::new(false);
        root_checks.set(0);
        let recovered = retry_missing_with_verified_root(
            Err::<usize, _>(Error::from(ErrorKind::NotFound)),
            || {
                root_checks.set(root_checks.get() + 1);
                held.set(true);
                Ok(RootLease(&held))
            },
            || {
                assert!(
                    held.get(),
                    "verified root must remain open during the second file open"
                );
                reopens.set(reopens.get() + 1);
                Ok(17)
            },
        )??;
        assert_eq!(recovered, 17);
        assert_eq!(root_checks.get(), 1);
        assert_eq!(reopens.get(), 1);
        assert!(!held.get());

        let absent = retry_missing_with_verified_root::<usize, ()>(
            Err(Error::from(ErrorKind::NotFound)),
            || Ok(()),
            || Err(Error::from(ErrorKind::NotFound)),
        )?;
        assert_eq!(absent.unwrap_err().kind(), ErrorKind::NotFound);
        let denied = retry_missing_with_verified_root::<usize, ()>(
            Err(Error::from(ErrorKind::PermissionDenied)),
            || panic!("ordinary open errors do not need another volume probe"),
            || panic!("ordinary open errors do not reopen the file"),
        )?;
        assert_eq!(denied.unwrap_err().kind(), ErrorKind::PermissionDenied);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_missing_guid_volume_is_deferred_and_recovers_without_a_new_event() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("volume.txt");
        fs::write(&path, "retainedvolumecontent")?;
        let mut available = job_for(path.clone())?;
        available.path = PathBuf::from(final_guid_path(&fs::File::open(&path)?)?);
        let key = DocKey::from_parts(available.volume_id, available.file_id);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::with_defaults();
        let args = args();
        process_batch(
            &stack,
            &index,
            &mut writer,
            durable_batch(vec![available.clone()]),
            &args,
        )?;
        assert_eq!(content_matches(&index, "retainedvolumecontent")?, 1);

        // A fresh random GUID has no mounted volume. This probes the real
        // Windows namespace without creating, dismounting or changing a drive.
        let mut unavailable = available.clone();
        unavailable.path = PathBuf::from(format!(r"\\?\Volume{{{}}}\volume.txt", Uuid::new_v4()));
        assert_eq!(
            fs::File::open(&unavailable.path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let pending = deferrable_batch(vec![unavailable]);
        process_batch(&stack, &index, &mut writer, pending.clone(), &args)?;
        let outcome = content_index::batch_outcome(&index.index)?.unwrap();
        assert!(outcome.receipt.complete);
        assert_eq!(Some(outcome.receipt.batch_id), pending.id);
        assert_eq!(outcome.deferred, vec![key]);
        assert_eq!(content_matches(&index, "retainedvolumecontent")?, 0);

        process_batch(&stack, &index, &mut writer, pending, &args)?;
        assert_eq!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred,
            vec![key]
        );
        process_batch(
            &stack,
            &index,
            &mut writer,
            deferrable_batch(vec![available]),
            &args,
        )?;
        assert!(
            content_index::batch_outcome(&index.index)?
                .unwrap()
                .deferred
                .is_empty()
        );
        assert_eq!(content_matches(&index, "retainedvolumecontent")?, 1);
        assert_eq!(content_index::open_reader(&index)?.searcher().num_docs(), 1);
        Ok(())
    }

    #[test]
    fn snapshot_rejects_metadata_changes_during_extraction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("snapshot.txt");
        fs::write(&path, "before")?;
        let job = job_for(path.clone())?;
        let before = probe_job_file(&job)?.expect("existing file");
        let metadata_writer = fs::OpenOptions::new().write(true).open(&path)?;
        metadata_writer.set_modified(before.modified + std::time::Duration::from_secs(2))?;
        drop(metadata_writer);
        let after = probe_job_file(&job)?.expect("existing file");
        assert!(!before.matches(&after));
        drop(after);
        drop(before);
        fs::OpenOptions::new().write(true).open(path)?;
        Ok(())
    }

    #[test]
    fn reconcile_reads_verified_handle_when_path_is_swapped_away_and_back() -> Result<()> {
        use content_extractor::{ExtractError, ExtractedContent, Extractor, SimpleTextExtractor};

        struct SwappingExtractor;
        impl SwappingExtractor {
            fn while_replaced(
                ctx: &ExtractContext,
                extract: impl FnOnce() -> std::result::Result<ExtractedContent, ExtractError>,
            ) -> std::result::Result<ExtractedContent, ExtractError> {
                let path = PathBuf::from(ctx.path);
                let held = path.with_extension("held");
                let replacement = path.with_extension("replacement");
                let io_error = |error: std::io::Error| ExtractError::Failed(error.to_string());
                fs::rename(&path, &held).map_err(io_error)?;
                fs::write(&path, "wrongsource").map_err(io_error)?;
                let result = extract();
                fs::rename(&path, replacement).map_err(io_error)?;
                fs::rename(held, path).map_err(io_error)?;
                result
            }
        }
        impl Extractor for SwappingExtractor {
            fn name(&self) -> &'static str {
                "path-swap-regression"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
            ) -> std::result::Result<ExtractedContent, ExtractError> {
                Self::while_replaced(ctx, || SimpleTextExtractor.extract(ctx, key))
            }
            fn extract_file(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                file: &fs::File,
            ) -> std::result::Result<ExtractedContent, ExtractError> {
                Self::while_replaced(ctx, || SimpleTextExtractor.extract_file(ctx, key, file))
            }
            fn extract_bytes(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                bytes: &[u8],
            ) -> std::result::Result<ExtractedContent, ExtractError> {
                Self::while_replaced(ctx, || SimpleTextExtractor.extract_bytes(ctx, key, bytes))
            }
        }

        let dir = tempfile::tempdir()?;
        let path = dir.path().join("identity.txt");
        fs::write(&path, "verifiedsource")?;
        let job = job_for(path.clone())?;
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::new(vec![Box::new(SwappingExtractor)]);
        process_job(&stack, &index, &mut writer, job, &args())?;
        writer.commit()?;
        assert_eq!(fs::read_to_string(path)?, "verifiedsource");
        assert_eq!(content_matches(&index, "verifiedsource")?, 1);
        assert_eq!(content_matches(&index, "wrongsource")?, 0);
        Ok(())
    }

    #[test]
    fn guid_roots_distinguish_actual_volumes_without_rejecting_case_aliases() {
        let first = r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}\file.txt";
        let alias = r"\\?\volume{AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE}\other.txt";
        let other = r"\\?\Volume{11111111-bbbb-cccc-dddd-eeeeeeeeeeee}\file.txt";
        assert!(
            volume_guid_root(first)
                .unwrap()
                .eq_ignore_ascii_case(volume_guid_root(alias).unwrap())
        );
        assert_ne!(volume_guid_root(first), volume_guid_root(other));
        for invalid in [
            r"C:\file.txt",
            r"\\server\share\file.txt",
            r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}",
            r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}suffix\file.txt",
            r"\\?\Volume{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeez}\file.txt",
        ] {
            assert!(volume_guid_root(invalid).is_none(), "{invalid}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_reconcile_rejects_path_replaced_by_another_frn() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("identity.txt");
        fs::write(&path, "original")?;
        let job = job_for(path.clone())?;
        fs::rename(&path, dir.path().join("retained.txt"))?;
        fs::write(path, "unrelated replacement")?;
        assert!(probe_job_file(&job)?.is_none());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_reconcile_verifies_guid_paths_and_volume_serial() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("volume.txt");
        fs::write(&path, "verified")?;
        let mut job = job_for(path)?;
        let file = fs::File::open(&job.path)?;
        job.path = PathBuf::from(final_guid_path(&file)?);
        assert!(volume_guid_root(job.path.to_str().unwrap()).is_some());
        let before = probe_job_file(&job)?.expect("GUID path resolves to the expected file");
        let mut after = probe_job_file(&job)?.expect("same file still exists");
        assert!(before.matches(&after));
        // A matching FRN alone cannot establish identity on a different volume.
        after.identity.volume_serial ^= 1;
        assert!(!before.matches(&after));
        Ok(())
    }

    #[test]
    fn job_file_supports_delete_without_path_and_empty_volume_reset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("jobs.json");
        fs::write(&path, br#"{"version":2,"jobs":[{"volume_id":7,"file_id":18446744073709551615,"operation":"delete"}]}"#)?;
        let batch = load_jobs(&path)?;
        assert_eq!(batch.jobs[0].operation, JobOperation::Delete);
        assert_eq!(batch.jobs[0].file_id, u64::MAX);
        assert!(batch.jobs[0].path.as_os_str().is_empty());
        fs::write(&path, br#"{"version":2,"reset_volumes":[7],"jobs":[]}"#)?;
        let batch = load_jobs(&path)?;
        assert_eq!(batch.reset_volumes, vec![7]);
        assert!(batch.jobs.is_empty());
        fs::write(
            &path,
            br#"{"version":1,"jobs":[{"volume_id":7,"file_id":1,"path":"missing.txt"}]}"#,
        )?;
        assert_eq!(load_jobs(&path)?.jobs[0].operation, JobOperation::Upsert);
        fs::write(&path, br#"{"version":2,"jobs":[{"volume_id":7,"file_id":1,"path":"gone.txt","operation":"reconcile"}]}"#)?;
        assert_eq!(load_jobs(&path)?.jobs[0].operation, JobOperation::Reconcile);
        Ok(())
    }

    #[test]
    fn new_job_operations_require_an_explicit_compatible_worker_protocol() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("jobs.json");
        for operation in ["delete", "reconcile"] {
            let job = serde_json::json!({"volume_id":7,"file_id":42,"path":"document.txt","operation":operation});
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"version":1,"jobs":[job.clone()]}))?,
            )?;
            assert!(
                load_jobs(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("version 2")
            );
            fs::write(&path, serde_json::to_vec(&vec![job])?)?;
            assert!(
                load_jobs(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("version 2")
            );
        }
        fs::write(&path, br#"{"version":1,"reset_volumes":[7],"jobs":[]}"#)?;
        assert!(
            load_jobs(&path)
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
        fs::write(
            &path,
            br#"[{"volume_id":7,"file_id":42,"path":"legacy.txt"}]"#,
        )?;
        assert_eq!(load_jobs(&path)?.jobs[0].operation, JobOperation::Upsert);
        fs::write(&path, br#"{"version":5,"jobs":[]}"#)?;
        assert!(
            load_jobs(&path)
                .unwrap_err()
                .to_string()
                .contains("unsupported job file version 5")
        );
        Ok(())
    }

    #[test]
    fn durable_receipts_require_version_three_and_a_nonnil_batch_id() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("jobs.json");
        for invalid in [
            serde_json::json!({"version":3,"jobs":[]}),
            serde_json::json!({"version":3,"id":null,"jobs":[]}),
            serde_json::json!({"version":3,"id":Uuid::nil(),"jobs":[]}),
            serde_json::json!({"version":4,"jobs":[]}),
            serde_json::json!({"version":4,"id":null,"jobs":[]}),
            serde_json::json!({"version":4,"id":Uuid::nil(),"jobs":[]}),
        ] {
            fs::write(&path, serde_json::to_vec(&invalid)?)?;
            assert!(
                load_jobs(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("nonnil batch id")
            );
        }
        let id = Uuid::new_v4();
        for version in [1, 2] {
            fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({
                    "version":version,
                    "id":id,
                    "jobs":[{"volume_id":7,"file_id":42,"path":"legacy.txt"}]
                }))?,
            )?;
            assert!(
                load_jobs(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("version 3")
            );
        }
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"version":3,"id":id,"jobs":[]}))?,
        )?;
        let batch = load_jobs(&path)?;
        assert_eq!(batch.id, Some(id));
        assert!(batch.jobs.is_empty());
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"version":4,"id":id,"jobs":[]}))?,
        )?;
        let batch = load_jobs(&path)?;
        assert_eq!(batch.version, 4);
        assert_eq!(batch.id, Some(id));
        let jobs = (0..=content_index::MAX_DEFERRED_KEYS)
            .map(|position| JobSpec {
                volume_id: 7,
                file_id: position as u64,
                path: PathBuf::from("bounded.txt"),
                operation: JobOperation::Reconcile,
                max_bytes: None,
                max_chars: None,
                file_size: 0,
            })
            .collect();
        let mut excessive = deferrable_batch(jobs);
        assert!(
            validate_job_file(&excessive)
                .unwrap_err()
                .to_string()
                .contains("deferrable identities")
        );
        excessive.jobs.pop();
        validate_job_file(&excessive)?;
        fs::write(&path, br#"{"version":2,"reset_volumes":[7]}"#)?;
        let legacy = load_jobs(&path)?;
        assert_eq!(legacy.id, None);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        content_index::commit_batch(&mut writer, id)?;
        process_batch(
            &ExtractorStack::with_defaults(),
            &index,
            &mut writer,
            legacy,
            &args(),
        )?;
        assert_eq!(content_index::committed_batch(&index.index)?, None);
        Ok(())
    }

    #[test]
    fn reconcile_growth_and_binary_content_keep_metadata_and_clear_old_text() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("changed.txt");
        fs::write(&path, "obsoleteword")?;
        let mut job = job_for(path.clone())?;
        let key = DocKey::from_parts(job.volume_id, job.file_id);
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::with_defaults();
        let args = args();
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "obsoleteword")?, 1);

        // The file grew after enqueue, so its old size hint cannot gate it out.
        job.file_size = 12;
        job.max_bytes = Some(16);
        fs::write(
            &path,
            "a much longer current document than the queued size hint",
        )?;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "obsoleteword")?, 0);
        let reader = content_index::open_reader(&index)?;
        let current = content_index::read_file_meta(&index, &reader, key)?.unwrap();
        assert_eq!(current.size, fs::metadata(&path)?.len());
        assert_eq!(reader.searcher().num_docs(), 1);

        job.max_bytes = None;
        fs::write(&path, b"\0\0binary\0\0")?;
        process_job(&stack, &index, &mut writer, job.clone(), &args)?;
        writer.commit()?;
        assert_eq!(content_matches(&index, "binary")?, 0);
        assert!(
            content_index::read_file_meta(&index, &content_index::open_reader(&index)?, key)?
                .is_some()
        );
        job.operation = JobOperation::Upsert;
        assert!(process_job(&stack, &index, &mut writer, job, &args).is_err());
        Ok(())
    }

    #[test]
    fn reconcile_does_not_hide_unknown_extraction_failures() -> Result<()> {
        struct FailingExtractor;
        impl content_extractor::Extractor for FailingExtractor {
            fn name(&self) -> &'static str {
                "failed-io"
            }
            fn supports(&self, _: &ExtractContext) -> bool {
                true
            }
            fn extract(
                &self,
                _: &ExtractContext,
                _: DocKey,
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                Err(content_extractor::ExtractError::Failed(
                    "interrupted read".into(),
                ))
            }
            fn extract_file(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                _: &fs::File,
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                self.extract(ctx, key)
            }
            fn extract_bytes(
                &self,
                ctx: &ExtractContext,
                key: DocKey,
                _: &[u8],
            ) -> std::result::Result<
                content_extractor::ExtractedContent,
                content_extractor::ExtractError,
            > {
                self.extract(ctx, key)
            }
        }
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("retry.txt");
        fs::write(&path, "current")?;
        let job = job_for(path)?;
        let index = content_index::create_in_ram()?;
        let mut writer = content_index::create_writer(
            &index,
            &WriterConfig {
                heap_size_bytes: 20_000_000,
                num_threads: 1,
            },
        )?;
        let stack = ExtractorStack::new(vec![Box::new(FailingExtractor)]);
        let error = process_job(&stack, &index, &mut writer, job, &args()).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<content_extractor::ExtractError>(),
            Some(content_extractor::ExtractError::Failed(_))
        ));
        Ok(())
    }
}
