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
    dotenvy::dotenv().ok();
    let args = Args::parse();

    #[cfg(windows)]
    if let Some(cmd) = args.command {
        match cmd {
            ServiceCommand::Install => return service::windows::management::install_service(),
            ServiceCommand::Uninstall => return service::windows::management::uninstall_service(),
            ServiceCommand::Start => return service::windows::management::start_service(),
            ServiceCommand::Stop => return service::windows::management::stop_service(),
        }
    }

    // Load config early to ensure it exists, though bootstrap will reload or use passed cfg.
    let cfg = load_or_create_config(None)?;

    tracing::info!("Starting service (console: {})", args.console);

    #[cfg(windows)]
    {
        if !args.console {
            // Attempt to start as a Windows Service.
            // This will block until the service stops.
            // We pass a dummy closure because our current skeleton hardcodes the bootstrap call inside service_main
            // to avoid static/global complexity for now.
            return service::windows::service_main::launch(|_| Ok(()));
        }
    }

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
