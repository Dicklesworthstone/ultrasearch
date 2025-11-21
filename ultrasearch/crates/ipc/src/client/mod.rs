#![cfg(target_os = "windows")]

mod named_pipe_client;

pub use named_pipe_client::PipeClient;
