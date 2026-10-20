use anyhow::Result;
use clap::{Parser, Subcommand};
use core_types::config::load_or_create_config;
use service::bootstrap;
use tokio::sync::mpsc;

#[derive(Parser, Debug)]
#[command(name = "ultrasearch-service", about = "UltraSearch Background Service")]
struct Args {
    #[command(subcommand)]
    command: Option<ServiceCommand>,

    /// Run in console mode (skip Service Control Manager hooks).
    #[arg(long, global = true)]
    console: bool,
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    /// Install the Windows Service
    Install,
    /// Uninstall the Windows Service
    Uninstall,
    /// Start the Windows Service
    Start,
    /// Stop the Windows Service
    Stop,
}

fn main() -> Result<()> {
    let args = Args::parse();

    #[cfg(windows)]
    if let Some(cmd) = args.command {
        dotenvy::dotenv().ok();
        match cmd {
            ServiceCommand::Install => return service::windows::management::install_service(),
            ServiceCommand::Uninstall => return service::windows::management::uninstall_service(),
            ServiceCommand::Start => return service::windows::management::start_service(),
            ServiceCommand::Stop => return service::windows::management::stop_service(),
        }
    }

    // Register with SCM before .env/config IO, ACL setup, or index work. A slow
    // or invalid configuration must be reported through the registered host,
    // not prevent the process from connecting to SCM in the first place.
    #[cfg(windows)]
    if !args.console {
        return service::windows::service_main::launch();
    }

    dotenvy::dotenv().ok();
    let cfg = load_or_create_config(None)?;
    // Ensure config file is writable by standard users for CLI/UI updates.
    let cfg_path = core_types::config::default_config_path();
    service::ensure_config_acl_writable(&cfg_path);

    tracing::info!("Starting service (console: {})", args.console);

    // Fallback (Linux or --console): run directly.
    tracing::info!("Running in console mode. Press Ctrl+C to stop.");

    let (tx, rx) = mpsc::channel(1);

    // Spawn a thread to catch Ctrl+C and signal shutdown
    std::thread::spawn(move || {
        // We build a minimal runtime just for the signal handler
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            rt.block_on(async {
                if tokio::signal::ctrl_c().await.is_ok() {
                    let _ = tx.send(()).await;
                }
            });
        }
    });

    bootstrap::run_app(&cfg, rx)
}
