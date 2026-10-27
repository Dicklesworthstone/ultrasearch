use std::env;
use std::ffi::OsString;
use std::thread;
use std::time::{Duration, Instant};

use super::service_main::SERVICE_NAME;
use anyhow::{Context, Result};
use windows_service::{
    service::{
        Service, ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceStatus, ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_LABEL: &str = "UltraSearch Background Service";
const SERVICE_DESC: &str = "Indexes files and provides fast search capabilities for UltraSearch.";
// Allow the host's 120-second startup grace plus its 20-second stop grace to
// report a terminal failure, instead of returning false success after 15 seconds.
const START_TIMEOUT: Duration = Duration::from_secs(150);
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

pub fn install_service() -> Result<()> {
    let exe_path = env::current_exe().context("failed to get current executable path")?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("failed to connect to service manager")?;

    let service_info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_LABEL),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe_path,
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };

    // We request CHANGE_CONFIG access on the returned handle so we can set the description immediately.
    let service = manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
        .context("failed to create service")?;

    // Set description
    if let Err(e) = service.set_description(SERVICE_DESC) {
        eprintln!("Warning: Failed to set service description: {}", e);
    }

    println!("Service '{}' installed successfully.", SERVICE_NAME);
    Ok(())
}

pub fn uninstall_service() -> Result<()> {
    // Best effort stop
    let _ = stop_service();

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to connect to service manager")?;

    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::DELETE)
        .context("failed to open service (does it exist?)")?;

    service.delete().context("failed to delete service")?;

    println!("Service '{}' uninstalled successfully.", SERVICE_NAME);
    Ok(())
}

pub fn start_service() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to connect to service manager")?;

    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::START | ServiceAccess::QUERY_STATUS,
        )
        .context("failed to open service")?;

    let status = service.query_status()?;
    if status.current_state == ServiceState::Running {
        println!("Service is already running.");
        return Ok(());
    }

    if status.current_state == ServiceState::Stopped {
        service
            .start::<&str>(&[])
            .context("failed to start service")?;
    } else if status.current_state != ServiceState::StartPending {
        anyhow::bail!(
            "cannot start {SERVICE_NAME} while it is {:?}; wait for the current transition to finish",
            status.current_state
        );
    }

    println!("Service '{}' starting...", SERVICE_NAME);

    wait_for_state(&service, ServiceState::Running, START_TIMEOUT)?;
    println!("Service started successfully; IPC is ready. Initial indexing may still be running.");
    Ok(())
}

pub fn stop_service() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to connect to service manager")?;

    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        )
        .context("failed to open service")?;

    let status = service.query_status()?;
    if status.current_state == ServiceState::Stopped {
        println!("Service is already stopped.");
        return Ok(());
    }

    if status.current_state != ServiceState::StopPending {
        service.stop().context("failed to stop service")?;
    }
    println!("Service '{}' stopping...", SERVICE_NAME);

    wait_for_state(&service, ServiceState::Stopped, STOP_TIMEOUT)?;
    println!("Service stopped.");
    Ok(())
}

fn wait_for_state(service: &Service, target: ServiceState, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = service.query_status()?;
        if transition_complete(&status, target)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "{SERVICE_NAME} did not reach {target:?} within {timeout:?}: state={:?}, checkpoint={}, exit={:?}. \
                 Inspect %PROGRAMDATA%\\UltraSearch\\log\\startup.log and the configured searchd.log",
                status.current_state,
                status.checkpoint,
                status.exit_code
            );
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn transition_complete(status: &ServiceStatus, target: ServiceState) -> Result<bool> {
    if status.current_state == target {
        return Ok(true);
    }
    if target == ServiceState::Running && status.current_state != ServiceState::StartPending {
        anyhow::bail!(
            "{SERVICE_NAME} failed to start: state={:?}, checkpoint={}, exit={:?}. \
             Inspect %PROGRAMDATA%\\UltraSearch\\log\\startup.log and the configured searchd.log",
            status.current_state,
            status.checkpoint,
            status.exit_code
        );
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_service::service::{ServiceControlAccept, ServiceExitCode};

    fn status(state: ServiceState, exit_code: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 2,
            wait_hint: Duration::from_secs(120),
            process_id: None,
        }
    }

    #[test]
    fn starting_only_succeeds_when_running_and_reports_early_failures() {
        assert!(
            !transition_complete(
                &status(ServiceState::StartPending, 0),
                ServiceState::Running
            )
            .unwrap()
        );
        assert!(
            transition_complete(&status(ServiceState::Running, 0), ServiceState::Running).unwrap()
        );
        for state in [ServiceState::Stopped, ServiceState::StopPending] {
            let error =
                transition_complete(&status(state, 1053), ServiceState::Running).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("failed to start"));
            assert!(message.contains("1053"));
            assert!(message.contains("startup.log"));
        }
    }

    #[test]
    fn stop_pending_is_not_a_successful_stop() {
        assert!(
            !transition_complete(&status(ServiceState::StopPending, 0), ServiceState::Stopped)
                .unwrap()
        );
        assert!(
            transition_complete(&status(ServiceState::Stopped, 0), ServiceState::Stopped).unwrap()
        );
    }
}
