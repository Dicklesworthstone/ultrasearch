use gpui::*;
use ipc::{SearchHit, SearchRequest, StatusResponse};
use std::sync::Arc;
use std::time::Duration;

use crate::ipc::client::IpcClient;

#[derive(Clone)]
pub struct SearchAppModel {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub status: Option<StatusResponse>,
    pub client: IpcClient,
}

impl SearchAppModel {
    pub fn new(cx: &mut AppContext) -> Model<Self> {
        let client = IpcClient::new();
        cx.new_model(|_| Self {
            query: String::new(),
            results: Vec::new(),
            status: None,
            client,
        })
    }

    pub fn set_query(&mut self, query: String, cx: &mut ModelContext<Self>) {
        self.query = query.clone();
        cx.notify();
        
        // In a real app, we would debounce here.
        // For now, fire immediately.
        let client = self.client.clone();
        let req = SearchRequest::with_query(ipc::QueryExpr::Term(ipc::TermExpr {
            field: None,
            value: query,
            modifier: ipc::TermModifier::Term, // Prefix would be better for live type
        }));

        cx.spawn(|this, mut cx| async move {
            if let Ok(resp) = client.search(req).await {
                this.update(&mut cx, |model, cx| {
                    model.results = resp.hits;
                    cx.notify();
                }).ok();
            }
        })
        .detach();
    }

    pub fn refresh_status(&mut self, cx: &mut ModelContext<Self>) {
        let client = self.client.clone();
        cx.spawn(|this, mut cx| async move {
            if let Ok(resp) = client.status(ipc::StatusRequest { id: uuid::Uuid::new_v4() }).await {
                this.update(&mut cx, |model, cx| {
                    model.status = Some(resp);
                    cx.notify();
                }).ok();
            }
        })
        .detach();
    }
}
