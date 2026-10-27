#![cfg(any(windows, test))]

mod named_pipe_client;

pub use named_pipe_client::{PipeClient, reconnect_success_count};
