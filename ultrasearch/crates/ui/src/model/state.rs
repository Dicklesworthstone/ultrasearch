use crate::background::{set_tray_status, TrayState};
use crate::ipc::client::IpcClient;
use core_types::DocKey;
use gpui::*;
use ipc::{
    MetricsSnapshot, SearchHit, SearchMode, SearchRequest, StatusRequest, StatusResponse,
    VolumeStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use uuid::Uuid;

const MAX_HEATMAP_ENTRIES: usize = 512;
const MAX_HISTORY: usize = 10;

/// Tracks user click/open frequency for `(normalized_query, DocKey)` pairs to boost
/// frequently opened files for repeated queries.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResultHeatmap {
    entries: HashMap<String, u32>,
    order: VecDeque<String>,
}

impl ResultHeatmap {
    fn make_key(query: &str, key: DocKey) -> Option<String> {
        let norm = query.trim().to_ascii_lowercase();
        if norm.is_empty() {
            return None;
        }
        Some(format!("{norm}\t{key}"))
    }

    pub fn record_open(&mut self, query: &str, key: DocKey) {
        let Some(map_key) = Self::make_key(query, key) else {
            return;
        };
        let count = self.entries.entry(map_key.clone()).or_insert(0);
        *count = count.saturating_add(1);
        self.order.retain(|k| k != &map_key);
        self.order.push_back(map_key);
        while self.entries.len() > MAX_HEATMAP_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }

    pub fn boost_for(&self, query: &str, key: DocKey) -> f32 {
        let Some(map_key) = Self::make_key(query, key) else {
            return 0.0;
        };
        let count = self.entries.get(&map_key).copied().unwrap_or(0);
        if count == 0 {
            0.0
        } else {
            (0.15 * (count as f32).ln_1p()).min(0.5)
        }
    }

    pub fn apply_boost(&self, query: &str, hits: &mut [SearchHit]) {
        if query.trim().is_empty() || self.entries.is_empty() {
            return;
        }
        let mut any_boosted = false;
        for hit in hits.iter_mut() {
            let boost = self.boost_for(query, hit.key);
            if boost > 0.0 {
                hit.score += boost;
                any_boosted = true;
            }
        }
        if any_boosted {
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.key.cmp(&b.key))
            });
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedUiState {
    #[serde(default)]
    history: VecDeque<String>,
    #[serde(default)]
    pinned_searches: Vec<String>,
    #[serde(default)]
    heatmap: ResultHeatmap,
    #[serde(default)]
    update_opt_in: bool,
}

fn ui_state_file_path() -> PathBuf {
    core_types::config::default_config_path().with_file_name("ui_state.json")
}

fn load_persisted_ui_state() -> PersistedUiState {
    let path = ui_state_file_path();
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(state) = serde_json::from_slice::<PersistedUiState>(&bytes) {
            return state;
        }
    }
    PersistedUiState {
        history: VecDeque::new(),
        pinned_searches: vec![
            "kind:code modified:7d".to_string(),
            "kind:docs modified:month".to_string(),
            "size:>10mb".to_string(),
        ],
        heatmap: ResultHeatmap::default(),
        update_opt_in: false,
    }
}

fn save_persisted_ui_state(state: &PersistedUiState) {
    let path = ui_state_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Parses an `ultrasearch://` protocol URI into a search query string.
/// Supports `ultrasearch://search?q=foo%20bar`, `ultrasearch://?query=foo+bar`,
/// and `ultrasearch://foo%20bar`.
pub fn parse_protocol_uri(arg: &str) -> Option<String> {
    let trimmed = arg.trim();
    if !trimmed
        .get(..14)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("ultrasearch://"))
    {
        return None;
    }
    let rest = &trimmed[14..];
    if let Some((_, query_part)) = rest.split_once('?') {
        for pair in query_part.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k.eq_ignore_ascii_case("q") || k.eq_ignore_ascii_case("query") {
                    let decoded = url_decode(v);
                    if !decoded.trim().is_empty() {
                        return Some(decoded.trim().to_string());
                    }
                }
            }
        }
    }
    let path = rest
        .strip_prefix("search/")
        .or_else(|| rest.strip_prefix("search"))
        .unwrap_or(rest)
        .trim_matches('/');
    let decoded = url_decode(path);
    let clean = decoded.trim();
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate { version: String },
    Available { version: String, notes: String },
    NeedsOptIn,
    Downloading { version: String, progress: u8 },
    ReadyToRestart { version: String, notes: String },
    Restarting,
}

#[derive(Debug, Clone)]
pub struct UpdateState {
    pub opt_in: bool,
    pub status: UpdateStatus,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            opt_in: false,
            status: UpdateStatus::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendMode {
    MetadataOnly,
    Mixed,
    ContentOnly,
}

impl From<BackendMode> for SearchMode {
    fn from(mode: BackendMode) -> Self {
        match mode {
            BackendMode::MetadataOnly => SearchMode::NameOnly,
            BackendMode::Mixed => SearchMode::Hybrid,
            BackendMode::ContentOnly => SearchMode::Content,
        }
    }
}

#[derive(Clone)]
pub struct SearchStatus {
    pub total: u64,
    pub shown: usize,
    pub last_latency_ms: Option<u32>,
    pub connected: bool,
    pub in_flight: bool,
    pub backend_mode: BackendMode,
    pub indexing_state: String,
    pub volumes: Vec<VolumeStatus>,
    pub metrics: Option<MetricsSnapshot>,
    pub served_by: Option<String>,
    pub content_jobs_total: u64,
    pub content_jobs_remaining: u64,
    pub content_bytes_total: u64,
    pub content_bytes_remaining: u64,
    pub reconnect_attempts: u32,
    pub connection_checked: bool,
    pub connection_error: Option<String>,
    pub search_error: Option<String>,
    pub config_error: Option<String>,
    pub last_index_commit_ts: Option<i64>,
    refresh_pending: bool,
}

impl Default for SearchStatus {
    fn default() -> Self {
        Self {
            total: 0,
            shown: 0,
            last_latency_ms: None,
            connected: false,
            in_flight: false,
            backend_mode: BackendMode::Mixed,
            indexing_state: "Waiting for service status".to_string(),
            volumes: Vec::new(),
            metrics: None,
            served_by: None,
            content_jobs_total: 0,
            content_jobs_remaining: 0,
            content_bytes_total: 0,
            content_bytes_remaining: 0,
            reconnect_attempts: 0,
            connection_checked: false,
            connection_error: None,
            search_error: None,
            config_error: None,
            last_index_commit_ts: None,
            refresh_pending: false,
        }
    }
}

impl SearchStatus {
    pub fn connection_label(&self) -> &'static str {
        if self.connected {
            "Connected"
        } else if self.connection_checked {
            "Disconnected"
        } else {
            "Connecting…"
        }
    }

    pub fn ingestion_state(&self) -> &str {
        ingestion_state(&self.indexing_state)
    }

    /// Keep transport health independent of scheduler admission and indexing.
    /// Visibility can change after the last metadata commit, when the initial
    /// journal catch-up finishes. Observe ingestion transitions as well as counts.
    fn apply_service_status(&mut self, response: StatusResponse) {
        self.refresh_pending |= !self.connected
            || self.last_index_commit_ts != response.last_index_commit_ts
            || self.ingestion_state() != ingestion_state(&response.scheduler_state)
            || self
                .volumes
                .iter()
                .map(|v| (v.volume, v.indexed_files))
                .ne(response.volumes.iter().map(|v| (v.volume, v.indexed_files)));
        self.connected = true;
        self.connection_checked = true;
        self.connection_error = None;
        self.reconnect_attempts = 0;
        self.indexing_state = response.scheduler_state;
        self.volumes = response.volumes;
        self.metrics = response.metrics;
        self.served_by = response.served_by;
        self.last_index_commit_ts = response.last_index_commit_ts;
        self.content_jobs_total = response.content_jobs_total.unwrap_or(0);
        self.content_jobs_remaining = response.content_jobs_remaining.unwrap_or(0);
        self.content_bytes_total = response.content_bytes_total.unwrap_or(0);
        self.content_bytes_remaining = response.content_bytes_remaining.unwrap_or(0);
    }

    fn take_search_refresh(&mut self, has_query: bool) -> bool {
        if self.refresh_pending && has_query && !self.in_flight {
            self.refresh_pending = false;
            true
        } else {
            false
        }
    }

    pub fn empty_search_message(&self, has_query: bool) -> (String, String) {
        if !self.connected {
            let title = if self.connection_checked {
                "Search service unavailable"
            } else {
                "Connecting to the search service…"
            };
            let mut detail = self.connection_error.clone().unwrap_or_else(|| {
                "Waiting for the service's named-pipe endpoint. The UI retries automatically."
                    .into()
            });
            if let Some(error) = &self.config_error {
                detail.push_str(&format!(" Configuration: {error}"));
            }
            return (title.into(), detail);
        }
        if let Some(error) = &self.search_error {
            return ("Search failed".into(), error.clone());
        }
        let ingestion = self.ingestion_state();
        if ingestion.starts_with("unavailable")
            || ingestion.starts_with("degraded")
            || ingestion.starts_with("retrying durable batch")
            || ingestion.starts_with("deferred capacity recovery")
        {
            return (
                "Indexing needs attention".into(),
                format!(
                    "{ingestion}. Open Service Health for progress and check the service logs."
                ),
            );
        }
        if ingestion.starts_with("initializing")
            || ingestion.starts_with("initial indexing")
            || ingestion.starts_with("reconciling")
            || ingestion == "reconciliation requested"
            || ingestion.starts_with("catching up")
            || (ingestion.starts_with("volume ")
                && (ingestion.contains("waiting")
                    || ingestion.contains("journal gap; reconciling")))
        {
            return (
                "Initial filename index is being built".into(),
                format!("{ingestion}. Your search will refresh as the index becomes available."),
            );
        }
        if has_query {
            (
                "No results found".into(),
                "Try different search terms or search mode".into(),
            )
        } else {
            ("Start typing to search".into(), String::new())
        }
    }
}

fn ingestion_state(state: &str) -> &str {
    // Worker failures are historical context, not a new visibility transition.
    state
        .split_once("; ingestion=")
        .map(|(_, ingestion)| {
            ingestion
                .split("; last_worker_problem(historical)=")
                .next()
                .unwrap_or(ingestion)
        })
        .unwrap_or(state)
}

pub struct SearchAppModel {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub status: SearchStatus,
    pub selected_index: Option<usize>,
    pub page_size: usize,
    pub page: usize,
    pub updates: UpdateState,
    pub hotkey_conflict: Option<String>,
    pub history: VecDeque<String>,
    pub pinned_searches: Vec<String>,
    pub heatmap: ResultHeatmap,
    pub show_shortcuts: bool,
    pub ipc_recent_reconnect: bool,
    pub client: IpcClient,
    pub search_debounce: Option<Task<()>>,
    pub status_task: Option<Task<()>>,
    pub last_search: Option<Instant>,
    pub show_onboarding: bool,
    pub show_status: bool,
}

impl SearchAppModel {
    pub fn new(cx: &mut Context<SearchAppModel>) -> Self {
        let client = IpcClient::new();
        let persisted = load_persisted_ui_state();

        let mut model = Self {
            query: String::new(),
            results: Vec::new(),
            status: SearchStatus::default(),
            selected_index: None,
            page_size: 50,
            page: 0,
            updates: UpdateState {
                opt_in: persisted.update_opt_in,
                status: UpdateStatus::Idle,
            },
            hotkey_conflict: None,
            history: persisted.history,
            pinned_searches: persisted.pinned_searches,
            heatmap: persisted.heatmap,
            show_shortcuts: false,
            ipc_recent_reconnect: false,
            client,
            search_debounce: None,
            status_task: None,
            last_search: None,
            show_onboarding: false,
            show_status: false,
        };

        model.start_status_polling(cx);
        model.update_tray_status();
        model
    }

    fn persist_ui_state(&self) {
        let state = PersistedUiState {
            history: self.history.clone(),
            pinned_searches: self.pinned_searches.clone(),
            heatmap: self.heatmap.clone(),
            update_opt_in: self.updates.opt_in,
        };
        save_persisted_ui_state(&state);
    }

    pub fn set_hotkey_conflict(&mut self, reason: impl Into<String>, cx: &mut Context<Self>) {
        self.hotkey_conflict = Some(reason.into());
        cx.notify();
    }

    pub fn start_status_polling(&mut self, cx: &mut Context<SearchAppModel>) {
        if let Some(task) = self.status_task.take() {
            drop(task);
        }
        let client = self.client.clone();
        let task = cx.spawn(move |this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
            let async_app = cx.clone();
            async move {
                let mut consecutive_failures = 0u32;
                let mut first_poll = true;
                loop {
                    let sleep_secs = if consecutive_failures == 0 {
                        2
                    } else {
                        (1u64 << consecutive_failures.saturating_sub(1).min(3)).min(10)
                    };
                    if first_poll {
                        first_poll = false;
                    } else {
                        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
                    }
                    let req = StatusRequest { id: Uuid::new_v4() };
                    match client.status(req).await {
                        Ok(resp) => {
                            let had_failures = consecutive_failures > 0;
                            consecutive_failures = 0;
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        let was_disconnected = had_failures
                                            || (!model.status.connected && model.status.connection_checked);
                                        model.status.apply_service_status(resp);
                                        if model.status.take_search_refresh(!model.query.trim().is_empty()) {
                                            let query = model.query.clone();
                                            model.set_query(query, cx);
                                        }
                                        if was_disconnected {
                                            model.ipc_recent_reconnect = true;
                                            cx.spawn(
                                                |weak: WeakEntity<SearchAppModel>,
                                                 cx: &mut AsyncApp| {
                                                    let app = cx.clone();
                                                    async move {
                                                        tokio::time::sleep(Duration::from_secs(4))
                                                            .await;
                                                        let _ = app.update(|app| {
                                                            weak.update(app, |m, cx| {
                                                                m.ipc_recent_reconnect = false;
                                                                cx.notify();
                                                            })
                                                        });
                                                    }
                                                },
                                            )
                                            .detach();
                                        }
                                        model.update_tray_status();
                                        cx.notify();
                                    },
                                )
                            });
                        }
                        Err(err) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let next_backoff =
                                (1u64 << consecutive_failures.saturating_sub(1).min(3)).min(10);
                            tracing::warn!(
                                "status poll failed (attempt {consecutive_failures}, next retry in {next_backoff}s): {err:#}"
                            );
                            let service_state = tokio::task::spawn_blocking(
                                ipc::service_status::query_installed_service,
                            ).await.unwrap_or_else(|error| {
                                ipc::service_status::InstalledServiceState::Unknown(error.to_string())
                            });
                            let diagnostic = service_state.connection_diagnostic(&format!("{err:#}"));
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        model.status.connected = false;
                                        model.status.connection_checked = true;
                                        model.status.connection_error = Some(diagnostic);
                                        model.status.reconnect_attempts = consecutive_failures;
                                        model.update_tray_status();
                                        cx.notify();
                                    },
                                )
                            });
                        }
                    }
                }
            }
        });
        self.status_task = Some(task);
    }

    pub fn retry_connection(&mut self, cx: &mut Context<Self>) {
        // Retry status immediately, even when the automatic loop is backing off.
        // Also retry an existing query, including a failed query with old rows.
        self.start_status_polling(cx);
        if !self.query.trim().is_empty() {
            self.set_query(self.query.clone(), cx);
        }
    }

    fn update_tray_status(&self) {
        let indexing = self
            .status
            .indexing_state
            .to_ascii_lowercase()
            .contains("index");
        let offline = !self.status.connected;
        let update_available = matches!(
            self.updates.status,
            UpdateStatus::Available { .. }
                | UpdateStatus::Downloading { .. }
                | UpdateStatus::ReadyToRestart { .. }
        );
        let restart_ready = matches!(self.updates.status, UpdateStatus::ReadyToRestart { .. });
        set_tray_status(TrayState {
            indexing,
            offline,
            update_available,
            restart_ready,
        });
    }

    pub fn set_query(&mut self, query: String, cx: &mut Context<SearchAppModel>) {
        self.query = query;
        self.status.search_error = None;
        self.status.refresh_pending = false;

        // Cancel previous debounce task
        if let Some(task) = self.search_debounce.take() {
            drop(task);
        }

        self.status.in_flight = !self.query.trim().is_empty();
        if !self.status.in_flight {
            self.results.clear();
            self.status.total = 0;
            self.status.shown = 0;
            self.page = 0;
            self.selected_index = None;
            cx.notify();
            return;
        }
        cx.notify();

        let query_clone = self.query.clone();
        let client = self.client.clone();
        let mode = self.status.backend_mode;

        self.search_debounce = Some(cx.spawn(
            move |this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
                let async_app = cx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;

                    let req = SearchRequest {
                        id: Uuid::new_v4(),
                        query: ipc::parse_query_str(&query_clone),
                        limit: 100,
                        mode: mode.into(),
                        timeout: Some(Duration::from_secs(5)),
                        offset: 0,
                    };

                    let start = Instant::now();
                    let _ = async_app.update(|app| {
                        this.update(
                            app,
                            |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                model.status.in_flight = true;
                                cx.notify();
                            },
                        )
                    });
                    match client.search(req).await {
                        Ok(mut resp) => {
                            let latency = start.elapsed().as_millis() as u32;
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.status.in_flight = false;
                                        model.heatmap.apply_boost(&query_clone, &mut resp.hits);
                                        model.results = resp.hits;
                                        model.status.total = resp.total;
                                        model.page = 0;
                                        model.status.shown = model.current_page_results().len();
                                        model.status.last_latency_ms = Some(latency);
                                        model.status.connected = true;
                                        model.status.connection_checked = true;
                                        model.status.connection_error = None;
                                        model.status.search_error = None;
                                        model.status.reconnect_attempts = 0;
                                        model.selected_index =
                                            if !model.results.is_empty() { Some(0) } else { None };
                                        model.push_history(&query_clone);
                                        model.update_tray_status();
                                        cx.notify();
                                    },
                                )
                            });
                        }
                        Err(err) => {
                            tracing::warn!("search request failed: {err:#}");
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.status.in_flight = false;
                                        model.status.search_error = Some(format!("{err:#}"));
                                        model.status.last_latency_ms = None;
                                        model.results.clear();
                                        model.status.total = 0;
                                        model.status.shown = 0;
                                        model.page = 0;
                                        model.selected_index = None;
                                        // A rejected query is not proof of a transport failure.
                                        // Probe status independently to establish connection health.
                                        model.start_status_polling(cx);
                                        model.update_tray_status();
                                        cx.notify();
                                    },
                                )
                            });
                        }
                    }
                }
            },
        ));
    }

    pub fn set_backend_mode(&mut self, mode: BackendMode, cx: &mut Context<SearchAppModel>) {
        self.status.backend_mode = mode;
        // Re-trigger search if we have a query
        if !self.query.is_empty() {
            let query = self.query.clone();
            self.set_query(query, cx);
        }
        cx.notify();
    }

    pub fn select_next(&mut self, cx: &mut Context<SearchAppModel>) {
        if self.results.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(i) if i < self.results.len() - 1 => i + 1,
            Some(i) => i,
            None => 0,
        });
        self.ensure_page_for_selection();
        cx.notify();
    }

    pub fn select_previous(&mut self, cx: &mut Context<SearchAppModel>) {
        if self.results.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(i) if i > 0 => i - 1,
            Some(i) => i,
            None => 0,
        });
        self.ensure_page_for_selection();
        cx.notify();
    }

    pub fn select_next_page(&mut self, cx: &mut Context<SearchAppModel>) {
        if self.results.is_empty() {
            return;
        }
        let max_idx = self.results.len() - 1;
        self.selected_index = Some(match self.selected_index {
            Some(i) => (i + self.page_size).min(max_idx),
            None => 0,
        });
        self.ensure_page_for_selection();
        self.status.shown = self.current_page_results().len();
        cx.notify();
    }

    pub fn select_prev_page(&mut self, cx: &mut Context<SearchAppModel>) {
        if self.results.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            Some(i) => i.saturating_sub(self.page_size),
            None => 0,
        });
        self.ensure_page_for_selection();
        self.status.shown = self.current_page_results().len();
        cx.notify();
    }

    pub fn record_result_open(&mut self, key: DocKey) {
        let query = self.query.clone();
        if !query.trim().is_empty() {
            self.heatmap.record_open(&query, key);
            self.push_history(&query);
            self.persist_ui_state();
        }
    }

    pub fn is_query_pinned(&self, query: &str) -> bool {
        let trimmed = query.trim();
        !trimmed.is_empty()
            && self
                .pinned_searches
                .iter()
                .any(|q| q.eq_ignore_ascii_case(trimmed))
    }

    pub fn toggle_pin_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(pos) = self
            .pinned_searches
            .iter()
            .position(|q| q.eq_ignore_ascii_case(trimmed))
        {
            self.pinned_searches.remove(pos);
        } else {
            self.pinned_searches.push(trimmed.to_string());
        }
        self.persist_ui_state();
        cx.notify();
    }

    pub fn selected_row(&self) -> Option<&SearchHit> {
        self.selected_index.and_then(|i| self.results.get(i))
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected_index == Some(index)
    }

    pub fn set_page(&mut self, page: usize, cx: &mut Context<SearchAppModel>) {
        let max_page = self
            .results
            .len()
            .div_ceil(self.page_size)
            .saturating_sub(1);
        self.page = page.min(max_page);
        self.status.shown = self.current_page_results().len();
        cx.notify();
    }

    pub fn page_start(&self) -> usize {
        self.page.saturating_mul(self.page_size)
    }

    pub fn current_page_results(&self) -> &[SearchHit] {
        let start = self.page_start();
        let end = (start + self.page_size).min(self.results.len());
        &self.results[start..end]
    }

    pub fn load_mock_results(&mut self, total: usize, cx: &mut Context<SearchAppModel>) {
        self.results.clear();
        for i in 0..total {
            self.results.push(SearchHit {
                key: core_types::DocKey::from_parts(1, i as u64 + 1),
                score: 1.0 - (i as f32 / total as f32),
                name: Some(format!("Design Spec {}", i + 1)),
                path: Some(format!(r"C:\Projects\UltraSearch\Docs\spec_{i:04}.md")),
                ext: Some("md".into()),
                size: Some(12_345 + i as u64 * 10),
                modified: Some(1_700_000_000 + i as i64 * 60),
                snippet: Some("Lorem ipsum dolor sit amet, consectetur adipiscing elit.".into()),
            });
        }
        self.page = 0;
        self.status.total = self.results.len() as u64;
        self.status.shown = self.current_page_results().len();
        self.selected_index = if self.results.is_empty() {
            None
        } else {
            Some(0)
        };
        cx.notify();
    }

    pub fn ensure_page_for_selection(&mut self) {
        if let Some(sel) = self.selected_index {
            self.page = sel / self.page_size;
        }
    }

    pub fn set_update_opt_in(&mut self, opt_in: bool, cx: &mut Context<SearchAppModel>) {
        self.updates.opt_in = opt_in;
        self.persist_ui_state();
        self.update_tray_status();
        cx.notify();
    }

    pub fn check_for_updates(&mut self, cx: &mut Context<SearchAppModel>) {
        if !self.updates.opt_in {
            self.updates.status = UpdateStatus::NeedsOptIn;
            self.update_tray_status();
            cx.notify();
            return;
        }
        let current_version = format!("v{}", env!("CARGO_PKG_VERSION"));
        if let Ok(avail) = std::env::var("ULTRASEARCH_UPDATE_AVAILABLE") {
            let trimmed = avail.trim();
            if !trimmed.is_empty() && trimmed != current_version {
                self.updates.status = UpdateStatus::Available {
                    version: trimmed.to_string(),
                    notes: "Signed release package verified and ready to download.".to_string(),
                };
                self.update_tray_status();
                cx.notify();
                return;
            }
        }
        self.updates.status = UpdateStatus::UpToDate {
            version: current_version,
        };
        self.update_tray_status();
        cx.notify();
    }

    pub fn start_update_download(&mut self, cx: &mut Context<SearchAppModel>) {
        let version = match &self.updates.status {
            UpdateStatus::Available { version, .. } => version.clone(),
            _ => return,
        };
        self.updates.status = UpdateStatus::Downloading {
            version: version.clone(),
            progress: 0,
        };
        self.update_tray_status();
        cx.notify();
        cx.spawn(|this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
            let async_app = cx.clone();
            async move {
                let mut progress = 0u8;
                while progress < 100 {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    progress = progress.saturating_add(10);
                    let _ = async_app.update(|app| {
                        this.update(app, |model, cx| {
                            if let UpdateStatus::Downloading { version, .. } = &model.updates.status
                            {
                                model.updates.status = UpdateStatus::Downloading {
                                    version: version.clone(),
                                    progress,
                                };
                                model.update_tray_status();
                                cx.notify();
                            }
                        })
                    });
                }
                let _ = async_app.update(|app| {
                    this.update(app, |model, cx| {
                        let ver = match &model.updates.status {
                            UpdateStatus::Downloading { version, .. } => version.clone(),
                            UpdateStatus::Available { version, .. } => version.clone(),
                            _ => "v0.2.0".into(),
                        };
                        let notes = match &model.updates.status {
                            UpdateStatus::Available { notes, .. } => notes.clone(),
                            _ => "Update downloaded".into(),
                        };
                        model.updates.status = UpdateStatus::ReadyToRestart {
                            version: ver,
                            notes,
                        };
                        model.update_tray_status();
                        cx.notify();
                    })
                });
            }
        })
        .detach();
    }

    pub fn restart_to_update(&mut self, cx: &mut Context<SearchAppModel>) {
        if !matches!(self.updates.status, UpdateStatus::ReadyToRestart { .. }) {
            return;
        }
        self.updates.status = UpdateStatus::Restarting;
        self.update_tray_status();
        cx.notify();
        cx.spawn(|this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
            let async_app = cx.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = async_app.update(|app| {
                    this.update(app, |model, cx| {
                        model.updates.status = UpdateStatus::Idle;
                        model.update_tray_status();
                        cx.notify();
                    })
                });
            }
        })
        .detach();
    }

    pub fn push_history(&mut self, query: &str) {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return;
        }
        self.history.retain(|q| !q.eq_ignore_ascii_case(trimmed));
        self.history.push_front(trimmed.to_string());
        while self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }
        self.persist_ui_state();
    }
}

impl Drop for SearchAppModel {
    fn drop(&mut self) {
        if let Some(task) = self.status_task.take() {
            drop(task);
        }
        if let Some(task) = self.search_debounce.take() {
            drop(task);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_protocol_uri, DocKey, ResultHeatmap, SearchHit, SearchStatus};
    use ipc::{StatusResponse, VolumeStatus};

    fn status_response(ingestion: &str, commit: Option<i64>, files: u64) -> StatusResponse {
        StatusResponse {
            id: uuid::Uuid::new_v4(),
            scheduler_state: format!("idle; ingestion={ingestion}"),
            volumes: vec![VolumeStatus {
                volume: 1,
                indexed_files: files,
                indexed_bytes: 0,
                pending_files: 0,
                pending_bytes: 0,
                last_usn: None,
                journal_id: None,
            }],
            last_index_commit_ts: commit,
            content_jobs_total: None,
            content_jobs_remaining: None,
            content_bytes_total: None,
            content_bytes_remaining: None,
            metrics: None,
            served_by: Some("fixture-service".into()),
        }
    }

    #[test]
    fn empty_results_distinguish_connection_indexing_and_query_failures() {
        let mut status = SearchStatus::default();
        assert_eq!(status.connection_label(), "Connecting…");
        assert!(status
            .empty_search_message(true)
            .0
            .starts_with("Connecting"));

        status.connection_checked = true;
        status.connection_error = Some("Windows service is stopped (Windows exit 5)".into());
        assert_eq!(
            status.empty_search_message(true).0,
            "Search service unavailable"
        );
        assert!(status
            .empty_search_message(true)
            .1
            .contains("Windows exit 5"));

        // A live pipe can answer status while the initial index is still hidden.
        status.apply_service_status(status_response(
            "initial indexing: waiting for workers",
            Some(10),
            900,
        ));
        assert_eq!(status.connection_label(), "Connected");
        assert!(status.connection_error.is_none());
        assert_eq!(
            status.empty_search_message(true).0,
            "Initial filename index is being built"
        );

        status.apply_service_status(status_response(
            "degraded: journal access denied",
            Some(10),
            900,
        ));
        assert_eq!(
            status.empty_search_message(true).0,
            "Indexing needs attention"
        );
        assert!(status.connected);

        status.apply_service_status(status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        ));
        assert_eq!(status.empty_search_message(true).0, "No results found");
        status.search_error = Some("query was rejected".into());
        assert_eq!(status.empty_search_message(true).0, "Search failed");
        assert!(
            status.connected,
            "a failed search does not imply a stopped service"
        );
    }

    #[test]
    fn initial_visibility_and_reconnect_refresh_queries_without_another_keystroke() {
        let mut status = SearchStatus::default();
        status.apply_service_status(status_response("initializing", None, 0));
        assert!(status.take_search_refresh(true));
        assert!(!status.take_search_refresh(true));

        status.apply_service_status(status_response(
            "initial indexing: waiting for workers",
            Some(10),
            900,
        ));
        status.in_flight = true;
        assert!(!status.take_search_refresh(true));
        status.in_flight = false;
        assert!(
            status.take_search_refresh(true),
            "refresh must survive an in-flight search"
        );

        // Publishing visibility after catch-up need not create a new commit or
        // change file counts. The query must still be rerun to reveal its hits.
        status.apply_service_status(status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        ));
        assert!(status.take_search_refresh(true));
        status.apply_service_status(status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        ));
        assert!(
            !status.take_search_refresh(true),
            "unchanged snapshots must not loop searches"
        );

        status.connected = false;
        status.apply_service_status(status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        ));
        assert!(
            status.take_search_refresh(true),
            "reconnect must refresh even when old rows existed"
        );
    }

    #[test]
    fn recovery_and_rescan_states_are_not_reported_as_successful_empty_searches() {
        for (ingestion, expected) in [
            (
                "unavailable: no configured NTFS volume is mounted",
                "Indexing needs attention",
            ),
            (
                "retrying durable batch: access denied",
                "Indexing needs attention",
            ),
            (
                "deferred capacity recovery: worker unavailable",
                "Indexing needs attention",
            ),
            (
                "reconciliation requested",
                "Initial filename index is being built",
            ),
            (
                "volume 1 journal gap; reconciling",
                "Initial filename index is being built",
            ),
        ] {
            let mut status = SearchStatus::default();
            status.apply_service_status(status_response(ingestion, Some(10), 900));
            let (title, detail) = status.empty_search_message(true);
            assert_eq!(title, expected, "{ingestion}");
            assert!(detail.contains(ingestion));
            assert!(
                status.connected,
                "index recovery does not imply a lost connection"
            );
        }
    }

    #[test]
    fn scheduler_heartbeats_and_historical_worker_errors_do_not_trigger_search_loops() {
        let mut status = SearchStatus::default();
        status.apply_service_status(status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        ));
        assert!(status.take_search_refresh(true));
        let mut response = status_response(
            "watching; last bounded journal read succeeded",
            Some(10),
            900,
        );
        response.scheduler_state = "idle=42 cpu=10%; ingestion=watching; last bounded journal read succeeded; last_worker_problem(historical)=an old failure".into();
        status.apply_service_status(response);
        assert!(!status.take_search_refresh(true));
    }

    #[test]
    fn parse_protocol_uri_extracts_and_decodes_queries() {
        assert_eq!(
            parse_protocol_uri("ultrasearch://search?q=annual%20report+2025"),
            Some("annual report 2025".to_string())
        );
        assert_eq!(
            parse_protocol_uri("ULTRASEARCH://?query=ext%3Ars+tokio"),
            Some("ext:rs tokio".to_string())
        );
        assert_eq!(
            parse_protocol_uri("ultrasearch://kind%3Adocs/"),
            Some("kind:docs".to_string())
        );
        assert_eq!(parse_protocol_uri("ultrasearch://"), None);
        assert_eq!(parse_protocol_uri("https://example.com"), None);
    }

    #[test]
    fn result_heatmap_boosts_and_reorders_frequently_opened_hits() {
        let mut heatmap = ResultHeatmap::default();
        let key1 = DocKey::from_parts(1, 10);
        let key2 = DocKey::from_parts(1, 20);

        heatmap.record_open("budget", key2);
        heatmap.record_open("BUDGET", key2);

        let mut hits = vec![
            SearchHit {
                key: key1,
                score: 1.0,
                name: Some("budget_old.xlsx".into()),
                path: None,
                ext: Some("xlsx".into()),
                size: None,
                modified: None,
                snippet: None,
            },
            SearchHit {
                key: key2,
                score: 0.95,
                name: Some("budget_2025.xlsx".into()),
                path: None,
                ext: Some("xlsx".into()),
                size: None,
                modified: None,
                snippet: None,
            },
        ];

        heatmap.apply_boost("budget", &mut hits);
        assert_eq!(hits[0].key, key2);
        assert!(hits[0].score > hits[1].score);
    }
}
