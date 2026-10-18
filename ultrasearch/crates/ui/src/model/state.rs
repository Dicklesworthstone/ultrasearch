use crate::background::{set_tray_status, TrayState};
use crate::ipc::client::IpcClient;
use core_types::DocKey;
use gpui::*;
use ipc::{MetricsSnapshot, SearchHit, SearchMode, SearchRequest, StatusRequest, VolumeStatus};
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
            indexing_state: "Idle".to_string(),
            volumes: Vec::new(),
            metrics: None,
            served_by: None,
            content_jobs_total: 0,
            content_jobs_remaining: 0,
            content_bytes_total: 0,
            content_bytes_remaining: 0,
            reconnect_attempts: 0,
        }
    }
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
                loop {
                    let sleep_secs = if consecutive_failures == 0 {
                        2
                    } else {
                        (1u64 << consecutive_failures.saturating_sub(1).min(3)).min(10)
                    };
                    tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
                    let req = StatusRequest { id: Uuid::new_v4() };
                    match client.status(req).await {
                        Ok(resp) => {
                            let had_failures = consecutive_failures > 0;
                            consecutive_failures = 0;
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        let was_disconnected =
                                            !model.status.connected || had_failures;
                                        model.status.connected = true;
                                        model.status.reconnect_attempts = 0;
                                        model.status.indexing_state = resp.scheduler_state.clone();
                                        model.status.volumes = resp.volumes;
                                        model.status.metrics = resp.metrics;
                                        model.status.served_by = resp.served_by;
                                        model.status.content_jobs_total =
                                            resp.content_jobs_total.unwrap_or(0);
                                        model.status.content_jobs_remaining =
                                            resp.content_jobs_remaining.unwrap_or(0);
                                        model.status.content_bytes_total =
                                            resp.content_bytes_total.unwrap_or(0);
                                        model.status.content_bytes_remaining =
                                            resp.content_bytes_remaining.unwrap_or(0);
                                        if was_disconnected {
                                            model.ipc_recent_reconnect = true;
                                            if !model.query.is_empty() && model.results.is_empty() {
                                                let q = model.query.clone();
                                                model.set_query(q, cx);
                                            }
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
                                "status poll failed (attempt {consecutive_failures}, next retry in {next_backoff}s): {err}"
                            );
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        model.status.connected = false;
                                        model.status.reconnect_attempts = consecutive_failures;
                                        model.status.indexing_state = format!(
                                            "Reconnecting (attempt {consecutive_failures}, retry in {next_backoff}s)…"
                                        );
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

        // Cancel previous debounce task
        if let Some(task) = self.search_debounce.take() {
            drop(task);
        }

        let query_clone = self.query.clone();
        let client = self.client.clone();
        let mode = self.status.backend_mode;

        self.search_debounce = Some(cx.spawn(
            move |this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
                let async_app = cx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;

                    if query_clone.is_empty() {
                        let _ = async_app.update(|app| {
                            this.update(
                                app,
                                |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                    model.results.clear();
                                    model.status.total = 0;
                                    model.status.shown = 0;
                                    model.page = 0;
                                    model.selected_index = None;
                                    cx.notify();
                                },
                            )
                        });
                        return;
                    }

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
                                        model.status.reconnect_attempts = 0;
                                        model.selected_index =
                                            if !model.results.is_empty() { Some(0) } else { None };
                                        model.push_history(&query_clone);
                                        cx.notify();
                                    },
                                )
                            });
                        }
                        Err(err) => {
                            tracing::warn!("search request failed: {err}");
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.status.in_flight = false;
                                        model.status.connected = false;
                                        model.status.indexing_state =
                                            "Disconnected (search)".to_string();
                                        model.status.last_latency_ms = None;
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
    use super::{parse_protocol_uri, DocKey, ResultHeatmap, SearchHit};

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
