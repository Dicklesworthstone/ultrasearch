use std::env;
use std::ffi::OsString;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_NAME: &str = "UltraSearch";
const SERVICE_LABEL: &str = "UltraSearch Background Service";
const SERVICE_DESC: &str = "Indexes files and provides fast search capabilities for UltraSearch.";

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

    service
        .start::<&str>(&[])
        .context("failed to start service")?;

    println!("Service '{}' starting...", SERVICE_NAME);

    // Wait for it to run
    for _ in 0..30 {
        thread::sleep(Duration::from_millis(500));
        let status = service.query_status()?;
        if status.current_state == ServiceState::Running {
            println!("Service started successfully.");
            return Ok(());
        }
    }

    println!("Service start initiated, but did not reach 'Running' state within timeout.");
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

    service.stop().context("failed to stop service")?;
    println!("Service '{}' stopping...", SERVICE_NAME);

    for _ in 0..30 {
        thread::sleep(Duration::from_millis(500));
        let status = service.query_status()?;
        if status.current_state == ServiceState::Stopped {
            println!("Service stopped.");
            return Ok(());
        }
    }

    println!("Service stop initiated, but did not reach 'Stopped' state within timeout.");
    Ok(())
}
