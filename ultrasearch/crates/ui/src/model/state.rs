use crate::ipc::client::IpcClient;
use gpui::*;
use ipc::{QueryExpr, SearchHit, SearchMode, SearchRequest, StatusRequest, TermExpr, TermModifier};
use std::time::{Duration, Instant};
use uuid::Uuid;

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
    pub backend_mode: BackendMode,
    pub indexing_state: String,
}

impl Default for SearchStatus {
    fn default() -> Self {
        Self {
            total: 0,
            shown: 0,
            last_latency_ms: None,
            connected: false,
            backend_mode: BackendMode::Mixed,
            indexing_state: "Idle".to_string(),
        }
    }
}

pub struct SearchAppModel {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub status: SearchStatus,
    pub selected_index: Option<usize>,
    pub client: IpcClient,
    pub search_debounce: Option<Task<()>>,
    pub status_task: Option<Task<()>>,
    pub last_search: Option<Instant>,
}

impl SearchAppModel {
    pub fn new(cx: &mut Context<SearchAppModel>) -> Self {
        let client = IpcClient::new();

        let mut model = Self {
            query: String::new(),
            results: Vec::new(),
            status: SearchStatus::default(),
            selected_index: None,
            client,
            search_debounce: None,
            status_task: None,
            last_search: None,
        };

        model.start_status_polling(cx);
        model
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
                    if let Ok(resp) = client.status(req).await {
                        let _ = async_app.update(|app| {
                            this.update(
                                app,
                                |model: &mut SearchAppModel, cx: &mut Context<SearchAppModel>| {
                                    model.status.connected = true;
                                    model.status.indexing_state = resp.scheduler_state.clone();
                                    cx.notify();
                                },
                            )
                        });
                    }
                }
            }
        });
        self.status_task = Some(task);
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
                    match client.search(req).await {
                        Ok(resp) => {
                            let latency = start.elapsed().as_millis() as u32;
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.results = resp.hits;
                                        model.status.total = resp.total;
                                        model.status.shown = model.results.len();
                                        model.status.last_latency_ms = Some(latency);
                                        model.status.connected = true;
                                        model.selected_index = if !model.results.is_empty() {
                                            Some(0)
                                        } else {
                                            None
                                        };
                                        cx.notify();
                                    },
                                )
                            });
                        }
                        Err(_) => {
                            let _ = async_app.update(|app| {
                                this.update(
                                    app,
                                    |model: &mut SearchAppModel,
                                     cx: &mut Context<SearchAppModel>| {
                                        model.status.connected = false;
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
        cx.notify();
    }

    pub fn selected_row(&self) -> Option<&SearchHit> {
        self.selected_index.and_then(|i| self.results.get(i))
    }

    pub fn is_selected(&self, index: usize) -> bool {
        self.selected_index == Some(index)
    }
}

impl Default for SearchAppModel {
    fn default() -> Self {
        panic!("SearchAppModel must be created with new(cx), not default()")
    }
}
