use std::ffi::OsString;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

const SERVICE_NAME: &str = "UltraSearch";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

pub fn run_service<F>(app_logic: F) -> Result<()>
where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + 'static,
{
    // The entry point where Windows calls us.
    // We need to wrap the closure to match the signature expected by define_windows_service!
    // but strictly speaking, define_windows_service! generates a static extern "system" fn.
    // So we typically use a static or a trampoline.
    // However, windows-service crate helper `service_dispatcher::start` takes a callback.

    // We'll use a channel to signal shutdown to the app logic.
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop => {
                let _ = shutdown_tx.try_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Register service control handler.
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)?;

    // Tell Windows we are starting.
    let next_status = ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::StartPending,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(next_status)?;

    // Tell Windows we are running.
    let next_status = ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(next_status)?;

    // Run the app logic (blocking this thread or spawning a runtime).
    // We load config here because we are in a clean thread/process context.
    // In a real service, environment variables might be tricky, so loading from file is best.
    let cfg = core_types::config::load_or_create_config(None)?;
    
    let result = crate::bootstrap::run_app(&cfg, shutdown_rx);

    // Report exit status.
    let exit_code = match result {
        Ok(_) => ServiceExitCode::Win32(0),
        Err(_) => ServiceExitCode::Win32(1),
    };

    let next_status = ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code,
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle.set_service_status(next_status)?;

    Ok(())
}

// Define the service entry point "main" for the Service Control Manager.
define_windows_service!(ffi_service_main, my_service_main);

fn my_service_main(_arguments: Vec<OsString>) {
    // This function is called by the SCM in a new thread.
    // But we designed `run_service` to take a closure, which is hard to pass through the static FFI boundary
    // without using global state.
    //
    // A common pattern is to have `run_service` be the thing called FROM main, and IT calls `service_dispatcher::start`.
    // BUT `service_dispatcher::start` takes the `ffi_service_main` extern fn.
    //
    // So we actually need a global or a way to pass the logic.
    // For simplicity in this skeleton, we might just assume the logic is "standard app logic"
    // and hardcode the call to a shared `run_app` from here, OR use a OnceLock to store the workload.
    //
    // However, to keep it clean, we'll expose a `launch` function that main.rs calls.
}

/// Called by main.rs when running as a service.
pub fn launch<F>(app_logic: F) -> Result<()>
where
    F: FnOnce(mpsc::Receiver<()>) -> Result<()> + Send + Sync + 'static,
{
    // Store the app logic in a static so the FFI callback can access it?
    // Actually, `windows-service` doesn't easily support passing closure to the service main.
    // We might need to invert this: `service_main` calls `app_logic` which we put in a global.
    
    // For now, let's simplify. We will just define the `ffi_service_main` here and have it call
    // a simpler "inner" service main that runs the reactor.
    
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.into())
}

// We need a place to stash the app logic if we want to be generic.
// But simpler: main.rs imports `service_main`, `service_main` imports `run_app` (or whatever) from a shared place?
// No, `run_app` is in `main.rs`. `main.rs` can't easily be imported by `lib.rs` (or submodules).
//
// Better approach: Move the `run_app` logic into `lib.rs` (or a module in lib) so both `main` (console) and `service_main` (service) can call it.
//
// Let's assume `crate::run_app_logic` is available or passed in.
// Since `windows-service` is strict, let's use a global `OnceLock` for the closure?
// No, closures are hard to store in statics.
//
// Alternative: Just hardcode the logic call in `my_service_main`.
// We can move the logic currently in `main.rs` to `lib.rs` (e.g. `bootstrap.rs`).
// Then `my_service_main` calls `bootstrap::run(shutdown_rx)`.

