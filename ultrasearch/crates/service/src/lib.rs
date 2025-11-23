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

#[cfg(all(test, target_os = "windows", feature = "e2e-windows"))]
mod e2e_windows_tests {
    use crate::bootstrap::{BootstrapOptions, run_app_with_options};
    use ::ipc::{
        QueryExpr, SearchMode, SearchRequest, StatusRequest, TermExpr, TermModifier,
        client::PipeClient,
    };
    use anyhow::Result;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2e_search_smoke() -> Result<()> {
        if std::env::var("ULTRASEARCH_E2E").as_deref() != Ok("1") {
            eprintln!("skipping e2e_search_smoke: set ULTRASEARCH_E2E=1 to enable");
            return Ok(());
        }

        // Resolve worker path if available so content jobs can run end-to-end.
        let worker_path = std::env::var("CARGO_BIN_EXE_search-index-worker")
            .or_else(|_| std::env::var("ULTRASEARCH_WORKER_PATH"))
            .ok();
        if let Some(ref path) = worker_path {
            std::env::set_var("ULTRASEARCH_WORKER_PATH", path);
        }

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
        std::fs::write(&file_path, b"hello ultrasearch e2e")?;
        let meta = FileMeta::new(
            DocKey::from_parts(1, 1),
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
            force_content_jobs: worker_path.is_some(),
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

        if worker_path.is_some() {
            // Poll for content results to confirm worker job execution.
            let mut content_found = false;
            for _ in 0..20 {
                let content_req: SearchRequest = SearchRequest {
                    id: Uuid::new_v4(),
                    query: QueryExpr::Term(TermExpr {
                        field: None,
                        value: "ultrasearch".into(),
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
                "content search should return results once worker runs"
            );
        }

        // Shutdown
        let _ = shutdown_tx.send(()).await;
        let _ = handle.join().expect("service thread panicked")?;
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

        // Seed content index with one doc.
        let content_idx = open_or_create(&content_index)?;
        let mut writer = create_writer(&content_idx, &WriterConfig::default())?;
        let doc = ContentDoc {
            key: DocKey::from_parts(1, 1),
            volume: 1,
            name: Some("hello.txt".into()),
            path: Some(r"C:\temp\hello.txt".into()),
            ext: Some("txt".into()),
            size: 20,
            modified: now_ts(),
            content_lang: Some("en".into()),
            content: "lorem ipsum ultrasearch content".into(),
        };
        add_content_doc(&mut writer, &content_idx.fields, &doc)?;
        writer.commit()?;

        // Seed meta index via bootstrap option.
        let meta = FileMeta::new(
            DocKey::from_parts(1, 1),
            1,
            None,
            "hello.txt".into(),
            Some(r"C:\temp\hello.txt".into()),
            20,
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
        let _ = handle.join().expect("service thread panicked")?;
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
        let _ = handle.join().expect("service thread panicked")?;
        Ok(())
    }
}
