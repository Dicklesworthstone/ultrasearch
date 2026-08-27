//! Windows Service Control Manager (SCM) host for the UltraSearch service.
//!
//! Lifecycle contract (issue #3 — the MSI's `ServiceControl Stop` used to
//! time out and `sc stop` reported 1061):
//!
//! - The control handler is registered before any status is reported, and
//!   `Running` is reported with STOP and SHUTDOWN accepted.
//! - The application itself runs on a worker thread. The service main thread
//!   only waits, so a long initial volume scan can never starve stop handling.
//! - A STOP/SHUTDOWN control immediately reports `StopPending` (with a wait
//!   hint) and signals the app. If the app does not wind down within
//!   [`STOP_GRACE`], the service reports `Stopped` anyway and returns; the
//!   process exit tears the worker down. An installer or `sc stop` therefore
//!   always observes a bounded, well-formed stop.
//! - A panic inside the app is caught so `Stopped` is still reported instead
//!   of the SCM seeing an unexplained process death.

use std::ffi::OsString;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult, ServiceStatusHandle},
    service_dispatcher,
};

/// Must match the `ServiceInstall/@Name` in the WiX package and the name used
/// by the `install`/`uninstall`/`start`/`stop` subcommands.
pub const SERVICE_NAME: &str = "UltraSearchService";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

/// How long a stop request waits for the application to wind down before the
/// service reports `Stopped` regardless and lets process exit clean up.
const STOP_GRACE: Duration = Duration::from_secs(20);
/// Wait hint advertised with `StopPending`; slightly above the grace period.
const STOP_WAIT_HINT: Duration = Duration::from_secs(30);
/// Polling cadence for the worker while the service is running.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

fn status(
    state: ServiceState,
    controls_accepted: ServiceControlAccept,
    exit_code: ServiceExitCode,
    checkpoint: u32,
    wait_hint: Duration,
) -> ServiceStatus {
    ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint,
        wait_hint,
        process_id: None,
    }
}

/// Outcome of the application worker thread.
type AppOutcome = std::thread::Result<Result<()>>;

fn exit_code_for(outcome: AppOutcome) -> ServiceExitCode {
    match outcome {
        Ok(Ok(())) => ServiceExitCode::Win32(0),
        Ok(Err(e)) => {
            eprintln!("UltraSearch service exited with error: {e:#}");
            ServiceExitCode::Win32(1)
        }
        Err(_) => {
            eprintln!("UltraSearch service worker panicked");
            ServiceExitCode::Win32(1)
        }
    }
}

pub fn run_service<F>(_app_logic: F) -> Result<()>
where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + 'static,
{
    // Signals the application (bootstrap::run_app) to wind down.
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    // Lets the control handler report StopPending through the handle that
    // only exists after registration.
    let status_slot: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());
    let stop_requested = Arc::new(AtomicBool::new(false));

    let handler_status = Arc::clone(&status_slot);
    let handler_stop = Arc::clone(&stop_requested);
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                if let Some(handle) = handler_status.get() {
                    let _ = handle.set_service_status(status(
                        ServiceState::StopPending,
                        ServiceControlAccept::empty(),
                        ServiceExitCode::Win32(0),
                        1,
                        STOP_WAIT_HINT,
                    ));
                }
                handler_stop.store(true, Ordering::SeqCst);
                let _ = shutdown_tx.try_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    let _ = status_slot.set(status_handle);

    status_handle.set_service_status(status(
        ServiceState::StartPending,
        ServiceControlAccept::empty(),
        ServiceExitCode::Win32(0),
        0,
        Duration::from_secs(10),
    ))?;

    // Run the application off the service main thread so control requests
    // are never queued behind config loading or the initial volume scan.
    let (done_tx, done_rx) = std_mpsc::channel::<AppOutcome>();
    thread::Builder::new()
        .name("ultrasearch-app".into())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                let cfg = core_types::config::load_or_create_config(None)?;
                crate::bootstrap::run_app(&cfg, shutdown_rx)
            }));
            let _ = done_tx.send(outcome);
        })?;

    status_handle.set_service_status(status(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
        ServiceExitCode::Win32(0),
        0,
        Duration::default(),
    ))?;

    let exit_code = loop {
        match done_rx.recv_timeout(POLL_INTERVAL) {
            Ok(outcome) => break exit_code_for(outcome),
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("UltraSearch service worker vanished without reporting");
                break ServiceExitCode::Win32(1);
            }
            Err(RecvTimeoutError::Timeout) => {
                if !stop_requested.load(Ordering::SeqCst) {
                    continue;
                }
                // Stop requested: give the app a bounded chance to finish
                // cleanly, then stop regardless so the SCM (and an MSI
                // uninstall) never waits on a worker stuck in a long scan.
                match done_rx.recv_timeout(STOP_GRACE) {
                    Ok(outcome) => break exit_code_for(outcome),
                    Err(_) => {
                        eprintln!(
                            "UltraSearch service worker did not stop within {STOP_GRACE:?}; \
                             reporting Stopped and exiting"
                        );
                        break ServiceExitCode::Win32(0);
                    }
                }
            }
        }
    };

    status_handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        exit_code,
        0,
        Duration::default(),
    ))?;

    Ok(())
}

// Define the service entry point "main" for the Service Control Manager.
define_windows_service!(ffi_service_main, my_service_main);

fn my_service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service(|_| Ok(())) {
        // No console under the SCM; stderr is the best available channel.
        eprintln!("Service failed: {e}");
    }
}

/// Called by main.rs when running as a service. Blocks until the service stops.
pub fn launch<F>(_app_logic: F) -> Result<()>
where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + Sync + 'static,
{
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.into())
}
