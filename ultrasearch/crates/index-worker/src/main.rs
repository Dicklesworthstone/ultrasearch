//! Index worker: extract files and write to the content index.
//!
//! Temporary shim until the full job contract lands. Supports:
//! - Single-file extraction (`--path` + volume/file ids)
//! - JSON job file (`--job-file`) containing an array of jobs
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
use std::path::PathBuf;
use std::{env, fs};
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Volume id for the document key (used when --job-file not supplied).
    #[arg(long)]
    volume_id: u16,
    /// File reference number (FRN) for the document key (used when --job-file not supplied).
    #[arg(long)]
    file_id: u64,
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
    path: PathBuf,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
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

    let mut args = Args::parse();

    // Allow env override for Extractous toggle.
    if let Ok(val) = env::var("ULTRASEARCH_ENABLE_EXTRACTOUS") {
        args.enable_extractous = matches!(val.as_str(), "1" | "true" | "TRUE");
    }

    let stack = ExtractorStack::with_extractous_enabled(args.enable_extractous);

    // Open index writer once for the run.
    let index: ContentIndex = content_index::open_or_create(&args.index_dir)?;
    let mut writer: IndexWriter = content_index::create_writer(&index, &WriterConfig::default())?;
    let mut pending = 0usize;

    if let Some(job_file) = args.job_file.clone() {
        let jobs = load_jobs(&job_file)?;
        for job in jobs {
            if let Err(err) = process_job(&stack, &index, &mut writer, job, &args) {
                warn!("job failed: {err}");
            }
            pending += 1;
            if args.commit_every > 0 && pending >= args.commit_every {
                writer.commit()?;
                pending = 0;
            }
        }
    } else {
        let path = args
            .path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--path is required when --job-file is not provided"))?;

        let single = JobSpec {
            volume_id: args.volume_id,
            file_id: args.file_id,
            path: path.clone(),
            max_bytes: Some(args.max_bytes),
            max_chars: Some(args.max_chars),
        };

        process_job(&stack, &index, &mut writer, single, &args)?;
        pending += 1;
    }

    if pending > 0 {
        writer.commit()?;
    }

    Ok(())
}

fn load_jobs(job_file: &PathBuf) -> Result<Vec<JobSpec>> {
    let file = fs::File::open(job_file)
        .with_context(|| format!("cannot open job file: {}", job_file.display()))?;
    // Prefer structured batch; fall back to legacy array for compatibility.
    #[derive(Debug, Deserialize)]
    struct JobFile {
        version: u32,
        jobs: Vec<JobSpec>,
    }

    match serde_json::from_reader::<_, JobFile>(&file) {
        Ok(batch) => {
            if batch.version != 1 {
                anyhow::bail!("unsupported job file version {}", batch.version);
            }
            Ok(batch.jobs)
        }
        Err(_) => {
            let file = fs::File::open(job_file)
                .with_context(|| format!("cannot re-open job file: {}", job_file.display()))?;
            let jobs: Vec<JobSpec> = serde_json::from_reader(file).with_context(|| {
                format!("failed to parse legacy job array: {}", job_file.display())
            })?;
            Ok(jobs)
        }
    }
}

fn process_job(
    stack: &ExtractorStack,
    index: &content_index::ContentIndex,
    writer: &mut IndexWriter,
    job: JobSpec,
    args: &Args,
) -> Result<()> {
    let doc_key = DocKey::from_parts(job.volume_id, job.file_id);

    // Choose per-job limits if present, otherwise fall back to CLI defaults.
    let max_bytes = job.max_bytes.unwrap_or(args.max_bytes);
    let max_chars = job.max_chars.unwrap_or(args.max_chars);

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

    let meta = fs::metadata(&job.path)
        .with_context(|| format!("file missing or unreadable: {}", job.path.display()))?;

    info!(
        "extracting {:?} (vol={}, frn={}) with extractous_enabled={} max_bytes={} max_chars={}",
        job.path, job.volume_id, job.file_id, args.enable_extractous, max_bytes, max_chars
    );

    match stack.extract(doc_key, &ctx) {
        Ok(out) => {
            let lang = out.lang.clone();
            let truncated = out.truncated;
            let bytes_processed = out.bytes_processed;
            let content_lang = out.content_lang.clone();

            info!(
                "extracted bytes={}, truncated={}, lang={:?}, content_lang={:?}",
                bytes_processed, truncated, out.lang, content_lang
            );

            // Index the document.
            let content_doc = to_content_doc(&job, &meta, out)?;
            let tdoc = content_index::to_document(&content_doc, &index.fields);
            writer.add_document(tdoc)?;

            // Output for debugging.
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
        Err(err) => Err(err),
    }
}

fn to_content_doc(
    job: &JobSpec,
    meta: &std::fs::Metadata,
    out: content_extractor::ExtractedContent,
) -> Result<content_index::ContentDoc> {
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
        modified,
        content_lang: out.content_lang.clone(),
        content: out.text,
    })
}
