use crate::ipc::client::IpcClient;
use ipc::{SearchHit, StatusResponse};

#[derive(Clone)]
pub struct SearchAppModel {
    pub query: String,
    pub results: Vec<SearchHit>,
    pub status: Option<StatusResponse>,
    pub client: IpcClient,
}

impl SearchAppModel {
    pub fn new() -> Self {
        let client = IpcClient::new();
        Self {
            query: String::new(),
            results: Vec::new(),
            status: None,
            client,
        }
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        // No async spawn here in simple mode
    }
}
