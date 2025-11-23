use crate::background::{set_tray_status, TrayState};
use crate::ipc::client::IpcClient;
use gpui::*;
use ipc::{
    MetricsSnapshot, QueryExpr, SearchHit, SearchMode, SearchRequest, StatusRequest, TermExpr,
    TermModifier, VolumeStatus,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateStatus {
    Idle,
    Checking,
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

        let mut model = Self {
            query: String::new(),
            results: Vec::new(),
            status: SearchStatus::default(),
            selected_index: None,
            page_size: 50,
            page: 0,
            updates: UpdateState::default(),
            hotkey_conflict: None,
            history: VecDeque::new(),
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
                loop {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    let req = StatusRequest { id: Uuid::new_v4() };
                    match client.status(req).await {
                        Ok(resp) => {
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        let was_disconnected = !model.status.connected;
                                        model.status.connected = true;
                                        model.status.indexing_state = resp.scheduler_state.clone();
                                        model.status.volumes = resp.volumes;
                                        model.status.metrics = resp.metrics;
                                        model.status.served_by = resp.served_by;
                                        if was_disconnected {
                                            model.ipc_recent_reconnect = true;
                                            cx.spawn(|weak: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
                                                let app = cx.clone();
                                                async move {
                                                    tokio::time::sleep(Duration::from_secs(4)).await;
                                                    let _ = app.update(|app| {
                                                        weak.update(app, |m, cx| {
                                                            m.ipc_recent_reconnect = false;
                                                            cx.notify();
                                                        })
                                                    });
                                                }
                                            })
                                            .detach();
                                        }
                                        model.update_tray_status();
                                        cx.notify();
                                    },
                                )
                            });
                        }
                        Err(err) => {
                            tracing::warn!("status poll failed: {err}");
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                        model.status.connected = false;
                                        model
                                            .status
                                            .indexing_state = "Disconnected (status)".to_string();
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
                        query: QueryExpr::Term(TermExpr {
                            field: None,
                            value: query_clone.clone(),
                            modifier: TermModifier::Term,
                        }),
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
                        Ok(resp) => {
                            let latency = start.elapsed().as_millis() as u32;
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.status.in_flight = false;
                                       model.results = resp.hits;
                                       model.status.total = resp.total;
                                        model.page = 0;
                                        model.status.shown = model.current_page_results().len();
                                        model.status.last_latency_ms = Some(latency);
                                        model.status.connected = true;
                                        model.selected_index =
                                            if !model.results.is_empty() { Some(0) } else { None };
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
        self.updates.status = UpdateStatus::Checking;
        self.update_tray_status();
        cx.notify();
        let client = self.client.clone();
        cx.spawn(|this: WeakEntity<SearchAppModel>, cx: &mut AsyncApp| {
            let async_app = cx.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                let fake_version = "v0.2.0".to_string();
                let fake_notes = "Performance improvements, UI polish, and bug fixes.".to_string();
                let _ = async_app.update(|app| {
                    this.update(app, |model, cx| {
                        let _ = &client; // reserved for real check call
                        model.updates.status = UpdateStatus::Available {
                            version: fake_version.clone(),
                            notes: fake_notes.clone(),
                        };
                        model.update_tray_status();
                        cx.notify();
                    })
                });
            }
        })
        .detach();
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
        if let Some(front) = self.history.front() {
            if front == trimmed {
                return;
            }
        }
        self.history.push_front(trimmed.to_string());
        const MAX_HISTORY: usize = 10;
        if self.history.len() > MAX_HISTORY {
            self.history.pop_back();
        }
    }
}

impl Default for SearchAppModel {
    fn default() -> Self {
        panic!("SearchAppModel must be created with new(cx), not default()")
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
