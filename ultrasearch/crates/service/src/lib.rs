//! Service support library: tracing/logging bootstrap and metrics helpers.

pub mod bootstrap;
pub mod dispatcher;
mod logging;
pub mod memory;
pub mod meta_ingest;
pub mod metrics;
pub mod planner;
pub mod priority;
pub mod scanner;
pub mod scheduler_runtime;
pub mod search_handler;
pub mod status;
pub mod status_provider;

#[cfg(windows)]
pub mod windows;

pub mod ipc; // I forgot to add this!

pub use logging::{init_tracing, init_tracing_with_config};
pub use meta_ingest::{ingest_file_meta_batch, ingest_with_paths};
pub use metrics::{
    ServiceMetrics, ServiceMetricsSnapshot, init_metrics_from_config, scrape_metrics,
};
pub use priority::{ProcessPriority, set_process_priority};
pub use scheduler_runtime::{SchedulerRuntime, set_live_active_workers, set_live_queue_counts};
pub use search_handler::{
    SearchHandler, StubSearchHandler, UnifiedSearchHandler, search, set_search_handler,
};
pub use status_provider::{
    BasicStatusProvider, init_basic_status_provider, set_status_provider, status_snapshot,
};

use core_types::config::AppConfig;
use ntfs_watcher::discover_volumes;
use std::fs;
use std::path::Path;
#[cfg(windows)]
use std::process::Command;

/// Ensure config has at least one volume; default to all discovered NTFS volumes if empty.
/// Best-effort persist back to the default config path, but proceed even if write fails.
pub fn ensure_default_volumes(cfg: &mut AppConfig) -> anyhow::Result<()> {
    if cfg.volumes.is_empty()
        && let Ok(vols) = discover_volumes()
    {
        let mounts: Vec<String> = vols
            .iter()
            .flat_map(|v| {
                v.drive_letters
                    .iter()
                    .map(|l| format!("{l}:\\"))
                    .collect::<Vec<_>>()
            })
            .collect();
        if !mounts.is_empty() {
            cfg.volumes = mounts.clone();
            if cfg.content_index_volumes.is_empty() {
                cfg.content_index_volumes = mounts;
            }
            persist_config(cfg);
        }
    }
    Ok(())
}

fn persist_config(cfg: &AppConfig) {
    let path = core_types::config::default_config_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(toml) = toml::to_string_pretty(cfg) {
        let _ = fs::write(&path, toml);
        ensure_config_acl_writable(&path);
    }
}

/// Best-effort: ensure Users have modify rights on the config file so the CLI/UI can update volumes.
pub fn ensure_config_acl_writable(path: &Path) {
    #[cfg(not(windows))]
    let _ = path;
    #[cfg(windows)]
    {
        let target = path.to_string_lossy().to_string();
        let parent = path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| target.clone());
        // Grant Modify to Users on the directory recursively so future files inherit.
        let _ = Command::new("icacls")
            .args([
                &parent,
                "/grant",
                "*S-1-5-32-545:(OI)(CI)M",
                "/T",
                "/C",
                "/Q",
            ])
            .status();
        // Also grant Modify on the file itself (in case it already exists).
        let _ = Command::new("icacls")
            .args([&target, "/grant", "*S-1-5-32-545:(M)", "/C", "/Q"])
            .status();
    }
}

#[cfg(all(test, target_os = "windows", feature = "e2e-windows"))]
mod e2e_windows_tests {
    use crate::bootstrap::{BootstrapOptions, run_app_with_options};
    use ::ipc::{
        QueryExpr, SearchMode, SearchRequest, StatusRequest, TermExpr, TermModifier,
        client::PipeClient,
    };
    use anyhow::{Context, Result, ensure};
    use content_index::{ContentDoc, WriterConfig, add_content_doc, create_writer, open_or_create};
    use core_types::{DocKey, FileFlags, FileMeta, Timestamp};
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use tokio::time::{Duration, sleep};
    use uuid::Uuid;

    fn now_ts() -> Timestamp {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn file_reference(path: &std::path::Path) -> Result<u64> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let file = std::fs::File::open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: file owns a valid handle and info remains writable for the call.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle() as isize), &mut info) }?;
        Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a real index-worker.exe and a retained E2E artifact directory"]
    async fn e2e_worker_failure_preserves_batch() -> Result<()> {
        use crate::dispatcher::job_dispatch::{JobDispatcher, JobSpec};

        ensure!(
            std::env::var("ULTRASEARCH_E2E").as_deref() == Ok("1"),
            "set ULTRASEARCH_E2E=1 before explicitly running the real-worker failure test"
        );
        let worker_path = std::env::var("ULTRASEARCH_WORKER_PATH")
            .context("set ULTRASEARCH_WORKER_PATH to the built index-worker.exe")?;
        ensure!(
            std::path::Path::new(&worker_path).is_file(),
            "real worker binary missing"
        );
        let artifact_dir = std::path::PathBuf::from(
            std::env::var_os("ULTRASEARCH_E2E_ARTIFACT_DIR")
                .context("set ULTRASEARCH_E2E_ARTIFACT_DIR to a retained test output directory")?,
        );
        ensure!(
            artifact_dir.is_dir(),
            "E2E artifact directory must already exist"
        );
        let root = artifact_dir.join(format!("worker-failure-{}", Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        eprintln!(
            "retaining real-worker failure artifacts at {}",
            root.display()
        );
        let input = root.join("document.txt");
        std::fs::write(&input, "real worker failure preservation input")?;
        // A regular file cannot become the Tantivy index directory. The real
        // worker must fail, rather than a stand-in simulating its exit status.
        let invalid_index = root.join("index-is-a-file");
        std::fs::write(&invalid_index, "preserve this incumbent file")?;
        let jobs_dir = root.join("jobs");
        let mut cfg = core_types::config::AppConfig::default();
        cfg.paths.content_index = invalid_index.to_string_lossy().into_owned();
        cfg.paths.jobs_dir = jobs_dir.to_string_lossy().into_owned();
        let job = JobSpec {
            operation: crate::dispatcher::job_dispatch::JobOperation::Upsert,
            volume_id: 1,
            file_id: 42,
            path: input.clone(),
            max_bytes: None,
            max_chars: None,
            file_size: std::fs::metadata(&input)?.len(),
        };

        let result = JobDispatcher::new(&cfg).spawn_batch(vec![job]).await;
        let failure = result
            .err()
            .context("failed real worker was reported as success")?;
        ensure!(
            failure.to_string().contains("failed with status"),
            "spawn/join failure does not qualify a real worker exit: {failure:#}"
        );
        let files = std::fs::read_dir(&jobs_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            files.len() == 1,
            "failed batch must remain available for recovery"
        );
        let batch: serde_json::Value = serde_json::from_slice(&std::fs::read(files[0].path())?)?;
        ensure!(batch["version"] == 3, "retained batch version changed");
        let batch_id: Uuid = serde_json::from_value(batch["id"].clone())?;
        ensure!(!batch_id.is_nil(), "retained batch identity is missing");
        let jobs = batch["jobs"].as_array().context("retained jobs missing")?;
        ensure!(jobs.len() == 1, "retained batch lost its job");
        let retained: JobSpec = serde_json::from_value(jobs[0].clone())?;
        ensure!(
            retained.path == input
                && retained.file_id == 42
                && retained.volume_id == 1
                && retained.file_size == std::fs::metadata(&input)?.len()
                && retained.max_bytes.is_none()
                && retained.max_chars.is_none(),
            "retained batch lost its input identity"
        );
        ensure!(
            std::fs::read(&invalid_index)? == b"preserve this incumbent file",
            "worker failure changed the incumbent index path"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a real index-worker.exe and a retained E2E artifact directory"]
    async fn e2e_mixed_worker_batch_preserves_failures_and_commits_successes() -> Result<()> {
        use crate::dispatcher::job_dispatch::{JobDispatcher, JobSpec};
        use tantivy::{collector::Count, query::QueryParser};

        ensure!(
            std::env::var("ULTRASEARCH_E2E").as_deref() == Ok("1"),
            "set ULTRASEARCH_E2E=1 before explicitly running the mixed real-worker batch test"
        );
        let worker_path = std::env::var("ULTRASEARCH_WORKER_PATH")
            .context("set ULTRASEARCH_WORKER_PATH to the built index-worker.exe")?;
        ensure!(
            std::path::Path::new(&worker_path).is_file(),
            "real worker binary missing"
        );
        let artifact_dir = std::path::PathBuf::from(
            std::env::var_os("ULTRASEARCH_E2E_ARTIFACT_DIR")
                .context("set ULTRASEARCH_E2E_ARTIFACT_DIR to a retained test output directory")?,
        );
        ensure!(
            artifact_dir.is_dir(),
            "E2E artifact directory must already exist"
        );
        let root = artifact_dir.join(format!("mixed-worker-batch-{}", Uuid::new_v4()));
        std::fs::create_dir(&root)?;
        eprintln!(
            "retaining mixed real-worker artifacts at {}",
            root.display()
        );
        let input = root.join("document.txt");
        let content_token = format!("content{}", Uuid::new_v4().simple());
        std::fs::write(&input, &content_token)?;
        let input_reference = file_reference(&input)?;
        let missing_input = root.join("missing-document.txt");
        let index_dir = root.join("content-index");
        std::fs::create_dir(&index_dir)?;
        let jobs_dir = root.join("jobs");
        let mut cfg = core_types::config::AppConfig::default();
        cfg.paths.content_index = index_dir.to_string_lossy().into_owned();
        cfg.paths.jobs_dir = jobs_dir.to_string_lossy().into_owned();
        let jobs = vec![
            JobSpec {
                operation: crate::dispatcher::job_dispatch::JobOperation::Upsert,
                volume_id: 1,
                file_id: input_reference,
                path: input.clone(),
                max_bytes: None,
                max_chars: None,
                file_size: std::fs::metadata(&input)?.len(),
            },
            JobSpec {
                operation: crate::dispatcher::job_dispatch::JobOperation::Upsert,
                volume_id: 1,
                file_id: 43,
                path: missing_input.clone(),
                max_bytes: Some(1024),
                max_chars: Some(1024),
                file_size: 0,
            },
        ];

        let result = JobDispatcher::new(&cfg).spawn_batch(jobs.clone()).await;
        let failure = result
            .err()
            .context("mixed batch with a missing input was reported as success")?;
        ensure!(
            failure.to_string().contains("failed with status"),
            "spawn/join failure does not qualify a real worker exit: {failure:#}"
        );
        let files = std::fs::read_dir(&jobs_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        ensure!(
            files.len() == 1,
            "mixed failed batch must remain recoverable"
        );
        let batch: serde_json::Value = serde_json::from_slice(&std::fs::read(files[0].path())?)?;
        ensure!(batch["version"] == 3, "retained batch version changed");
        let batch_id: Uuid = serde_json::from_value(batch["id"].clone())?;
        ensure!(!batch_id.is_nil(), "retained batch identity is missing");
        ensure!(
            batch["jobs"] == serde_json::to_value(&jobs)?,
            "retained mixed batch lost or changed an input"
        );
        ensure!(
            !missing_input.exists(),
            "missing input unexpectedly appeared"
        );
        ensure!(
            std::fs::read_to_string(&input)? == content_token,
            "worker changed the successful input"
        );

        // No test helper inserts documents: only the real worker can make the
        // fresh content-only token searchable despite the other item's error.
        let index = open_or_create(&index_dir)?;
        let receipt = content_index::batch_receipt(&index.index)?;
        ensure!(
            receipt.is_some_and(|receipt| receipt.batch_id == batch_id && !receipt.complete),
            "partial real-worker commit lost its durable batch identity"
        );
        let reader = content_index::open_reader(&index)?;
        reader.reload()?;
        let searcher = reader.searcher();
        let query = QueryParser::for_index(&index.index, vec![index.fields.content])
            .parse_query(&content_token)?;
        ensure!(
            searcher.search(&query, &Count)? == 1 && searcher.num_docs() == 1,
            "successful item was not committed exactly once before the failed batch exit"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires ULTRASEARCH_E2E=1 and a real built index-worker.exe"]
    async fn e2e_search_smoke() -> Result<()> {
        ensure!(
            std::env::var("ULTRASEARCH_E2E").as_deref() == Ok("1"),
            "set ULTRASEARCH_E2E=1 before explicitly running the real-worker smoke test"
        );
        // The worker belongs to a different package, so Cargo does not supply
        // CARGO_BIN_EXE for it to this library test. Require the real artifact
        // before starting any service thread; never silently omit content proof.
        let worker_path = std::env::var("ULTRASEARCH_WORKER_PATH")
            .context("set ULTRASEARCH_WORKER_PATH to the built index-worker.exe")?;
        ensure!(
            std::path::Path::new(&worker_path).is_file(),
            "real worker binary missing at {worker_path}"
        );

        let temp = tempdir()?;
        let data_dir = temp.path().join("data");
        let _ = std::fs::create_dir_all(&data_dir);
        let index_root = data_dir.join("index");
        let meta_index = index_root.join("meta");
        let content_index = index_root.join("content");
        let state_dir = data_dir.join("state");
        let jobs_dir = data_dir.join("jobs");
        let log_dir = data_dir.join("log");
        let _ = std::fs::create_dir_all(&meta_index);
        let _ = std::fs::create_dir_all(&content_index);
        let _ = std::fs::create_dir_all(&state_dir);
        let _ = std::fs::create_dir_all(&jobs_dir);
        let _ = std::fs::create_dir_all(&log_dir);

        // Create test document
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir_all(&docs_dir)?;
        let file_path = docs_dir.join("hello.txt");
        let content_token = format!("content{}", Uuid::new_v4().simple());
        std::fs::write(&file_path, format!("hello {content_token} e2e"))?;
        let meta = FileMeta::new(
            DocKey::from_parts(1, file_reference(&file_path)?),
            1,
            None,
            file_path.file_name().unwrap().to_string_lossy().to_string(),
            Some(file_path.to_string_lossy().to_string()),
            std::fs::metadata(&file_path)?.len(),
            now_ts(),
            now_ts(),
            FileFlags::empty(),
        );

        let mut cfg = core_types::config::AppConfig::default();
        cfg.app.data_dir = data_dir.to_string_lossy().to_string();
        cfg.logging.file = log_dir.join("searchd.log").to_string_lossy().to_string();
        cfg.paths.meta_index = meta_index.to_string_lossy().to_string();
        cfg.paths.content_index = content_index.to_string_lossy().to_string();
        cfg.paths.state_dir = state_dir.to_string_lossy().to_string();
        cfg.paths.jobs_dir = jobs_dir.to_string_lossy().to_string();
        cfg.metrics.enabled = false; // avoid binding ports in tests

        let pipe_name = format!(r"\\.\pipe\ultrasearch-test-{}", Uuid::new_v4());
        let opts = BootstrapOptions {
            initial_metas: Some(vec![meta]),
            skip_initial_ingest: true,
            pipe_name: Some(pipe_name.clone()),
            force_content_jobs: true,
        };

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let cfg_for_thread = cfg.clone();
        let handle =
            std::thread::spawn(move || run_app_with_options(&cfg_for_thread, shutdown_rx, opts));

        // Wait for pipe to become ready
        let client =
            PipeClient::new(pipe_name.clone()).with_request_timeout(Duration::from_millis(500));
        let mut ready = false;
        sleep(Duration::from_millis(150)).await;
        for _ in 0..20 {
            let req: StatusRequest = StatusRequest { id: Uuid::new_v4() };
            if client.status(req).await.is_ok() {
                ready = true;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        assert!(ready, "IPC server did not become ready in time");

        // Execute search
        let search_req: SearchRequest = SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: None,
                value: "hello".into(),
                modifier: TermModifier::Term,
            }),
            limit: 10,
            mode: SearchMode::NameOnly,
            timeout: Some(Duration::from_secs(2)),
            offset: 0,
        };
        let resp = client.search(search_req).await?;
        assert!(
            resp.total >= 1 && !resp.hits.is_empty(),
            "expected at least one indexed document, got total={} hits={}",
            resp.total,
            resp.hits.len()
        );

        // Only the real worker can put this fresh, content-only token in the
        // index. A filename hit or a pre-seeded fixture cannot satisfy the test.
        let mut content_found = false;
        for _ in 0..20 {
            let content_req: SearchRequest = SearchRequest {
                id: Uuid::new_v4(),
                query: QueryExpr::Term(TermExpr {
                    field: None,
                    value: content_token.clone(),
                    modifier: TermModifier::Term,
                }),
                limit: 10,
                mode: SearchMode::Content,
                timeout: Some(Duration::from_secs(2)),
                offset: 0,
            };
            let resp = client.search(content_req).await?;
            if resp.total > 0 {
                content_found = true;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        assert!(
            content_found,
            "content search should return the generated token once the real worker runs"
        );

        // Shutdown
        let _ = shutdown_tx.send(()).await;
        handle.join().expect("service thread panicked")?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2e_content_search() -> Result<()> {
        if std::env::var("ULTRASEARCH_E2E").as_deref() != Ok("1") {
            eprintln!("skipping e2e_content_search: set ULTRASEARCH_E2E=1 to enable");
            return Ok(());
        }

        let temp = tempdir()?;
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let index_root = data_dir.join("index");
        let meta_index = index_root.join("meta");
        let content_index = index_root.join("content");
        let state_dir = data_dir.join("state");
        let jobs_dir = data_dir.join("jobs");
        let log_dir = data_dir.join("log");
        for p in [&meta_index, &content_index, &state_dir, &jobs_dir, &log_dir] {
            std::fs::create_dir_all(p)?;
        }

        let mut cfg = core_types::config::AppConfig::default();
        cfg.app.data_dir = data_dir.to_string_lossy().to_string();
        cfg.logging.file = log_dir.join("searchd.log").to_string_lossy().to_string();
        cfg.paths.meta_index = meta_index.to_string_lossy().to_string();
        cfg.paths.content_index = content_index.to_string_lossy().to_string();
        cfg.paths.state_dir = state_dir.to_string_lossy().to_string();
        cfg.paths.jobs_dir = jobs_dir.to_string_lossy().to_string();
        cfg.metrics.enabled = false;
        // Prepare the coordinated generation before adding fixture documents;
        // startup correctly rebuilds indices that have no durable generation.
        drop(crate::scanner::initialize_indexes(&cfg)?);
        let file_path = temp.path().join("hello.txt");
        std::fs::write(&file_path, "lorem ipsum ultrasearch content")?;
        let key = DocKey::from_parts(1, file_reference(&file_path)?);
        let file_size = std::fs::metadata(&file_path)?.len();

        // Seed content index with one doc.
        let content_idx = open_or_create(&content_index)?;
        let mut writer = create_writer(&content_idx, &WriterConfig::default())?;
        let doc = ContentDoc {
            key,
            volume: 1,
            name: Some("hello.txt".into()),
            path: Some(file_path.to_string_lossy().into_owned()),
            ext: Some("txt".into()),
            size: file_size,
            created: now_ts(),
            modified: now_ts(),
            flags: 0,
            content_lang: Some("en".into()),
            content: "lorem ipsum ultrasearch content".into(),
        };
        add_content_doc(&mut writer, &content_idx.fields, &doc)?;
        writer.commit()?;
        drop(writer);

        // Seed meta index via bootstrap option.
        let meta = FileMeta::new(
            key,
            1,
            None,
            "hello.txt".into(),
            Some(file_path.to_string_lossy().into_owned()),
            file_size,
            now_ts(),
            now_ts(),
            FileFlags::empty(),
        );

        let pipe_name = format!(r"\\.\pipe\ultrasearch-test-{}", Uuid::new_v4());
        let opts = BootstrapOptions {
            initial_metas: Some(vec![meta]),
            skip_initial_ingest: true,
            pipe_name: Some(pipe_name.clone()),
            force_content_jobs: false,
        };

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let cfg_for_thread = cfg.clone();
        let handle =
            std::thread::spawn(move || run_app_with_options(&cfg_for_thread, shutdown_rx, opts));

        let client =
            PipeClient::new(pipe_name.clone()).with_request_timeout(Duration::from_millis(750));
        let mut ready = false;
        sleep(Duration::from_millis(150)).await;
        for _ in 0..20 {
            let req: StatusRequest = StatusRequest { id: Uuid::new_v4() };
            if client.status(req).await.is_ok() {
                ready = true;
                break;
            }
            sleep(Duration::from_millis(200)).await;
        }
        assert!(ready, "IPC server did not become ready in time (content)");

        let search_req: SearchRequest = SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: None,
                value: "lorem".into(),
                modifier: TermModifier::Term,
            }),
            limit: 5,
            mode: SearchMode::Content,
            timeout: Some(Duration::from_secs(2)),
            offset: 0,
        };
        let resp = client.search(search_req).await?;
        assert!(
            resp.total >= 1 && !resp.hits.is_empty(),
            "content search should return seeded doc; total={} hits={}",
            resp.total,
            resp.hits.len()
        );

        let _ = shutdown_tx.send(()).await;
        handle.join().expect("service thread panicked")?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2e_ipc_malformed_frame_resilience() -> Result<()> {
        if std::env::var("ULTRASEARCH_E2E").as_deref() != Ok("1") {
            eprintln!(
                "skipping e2e_ipc_malformed_frame_resilience: set ULTRASEARCH_E2E=1 to enable"
            );
            return Ok(());
        }

        let temp = tempdir()?;
        let data_dir = temp.path().join("data");
        std::fs::create_dir_all(&data_dir)?;
        let index_root = data_dir.join("index");
        let meta_index = index_root.join("meta");
        let content_index = index_root.join("content");
        let state_dir = data_dir.join("state");
        let jobs_dir = data_dir.join("jobs");
        let log_dir = data_dir.join("log");
        for p in [&meta_index, &content_index, &state_dir, &jobs_dir, &log_dir] {
            std::fs::create_dir_all(p)?;
        }

        let meta = FileMeta::new(
            DocKey::from_parts(1, 1),
            1,
            None,
            "alive.txt".into(),
            Some(r"C:\temp\alive.txt".into()),
            5,
            now_ts(),
            now_ts(),
            FileFlags::empty(),
        );

        let mut cfg = core_types::config::AppConfig::default();
        cfg.app.data_dir = data_dir.to_string_lossy().to_string();
        cfg.logging.file = log_dir.join("searchd.log").to_string_lossy().to_string();
        cfg.paths.meta_index = meta_index.to_string_lossy().to_string();
        cfg.paths.content_index = content_index.to_string_lossy().to_string();
        cfg.paths.state_dir = state_dir.to_string_lossy().to_string();
        cfg.paths.jobs_dir = jobs_dir.to_string_lossy().to_string();
        cfg.metrics.enabled = false;

        let pipe_name = format!(r"\\.\pipe\ultrasearch-test-{}", Uuid::new_v4());
        let opts = BootstrapOptions {
            initial_metas: Some(vec![meta]),
            skip_initial_ingest: true,
            pipe_name: Some(pipe_name.clone()),
            force_content_jobs: false,
        };

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let cfg_for_thread = cfg.clone();
        let handle =
            std::thread::spawn(move || run_app_with_options(&cfg_for_thread, shutdown_rx, opts));

        let client =
            PipeClient::new(pipe_name.clone()).with_request_timeout(Duration::from_millis(500));
        let mut ready = false;
        sleep(Duration::from_millis(150)).await;
        for _ in 0..20 {
            let req: StatusRequest = StatusRequest { id: Uuid::new_v4() };
            if client.status(req).await.is_ok() {
                ready = true;
                break;
            }
            sleep(Duration::from_millis(150)).await;
        }
        assert!(
            ready,
            "IPC server did not become ready in time (malformed test)"
        );

        // Send malformed frame (length=0)
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            let mut conn = ClientOptions::new().open(&pipe_name)?;
            conn.write_all(&0u32.to_le_bytes()).await?;
            let _ = conn.shutdown().await;
        }

        // Server should still respond to a valid request on a fresh connection.
        let search_req: SearchRequest = SearchRequest {
            id: Uuid::new_v4(),
            query: QueryExpr::Term(TermExpr {
                field: None,
                value: "alive".into(),
                modifier: TermModifier::Term,
            }),
            limit: 5,
            mode: SearchMode::NameOnly,
            timeout: Some(Duration::from_secs(2)),
            offset: 0,
        };
        let resp = client.search(search_req).await?;
        assert!(
            resp.total >= 1,
            "expected service to remain alive after malformed frame"
        );

        let _ = shutdown_tx.send(()).await;
        handle.join().expect("service thread panicked")?;
        Ok(())
    }
}
