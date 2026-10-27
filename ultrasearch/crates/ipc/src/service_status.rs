//! Read-only service registration diagnostics shared by the launcher and clients.
//!
//! A missing pipe does not prove the installed service is stopped: SCM may still
//! be starting it, or a running service may have an IPC failure. Only an explicit
//! `ERROR_SERVICE_DOES_NOT_EXIST` permits the portable launcher to own a console
//! service process.

pub const SERVICE_NAME: &str = "UltraSearchService";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledServiceState {
    NotInstalled,
    Stopped {
        win32_exit_code: u32,
        service_exit_code: u32,
    },
    Starting {
        checkpoint: u32,
        wait_hint_ms: u32,
    },
    Running,
    Stopping,
    Paused,
    Other(u32),
    Unknown(String),
}

impl InstalledServiceState {
    /// Never start a competing index writer when registration cannot be queried.
    pub fn allows_console_fallback(&self) -> bool {
        matches!(self, Self::NotInstalled)
    }

    pub fn summary(&self) -> String {
        match self {
            Self::NotInstalled => "Windows service is not installed".into(),
            Self::Stopped {
                win32_exit_code,
                service_exit_code,
            } => format!(
                "Windows service is stopped (Windows exit {win32_exit_code}, service exit {service_exit_code})"
            ),
            Self::Starting {
                checkpoint,
                wait_hint_ms,
            } => format!(
                "Windows service is starting (checkpoint {checkpoint}, wait hint {wait_hint_ms} ms)"
            ),
            Self::Running => "Windows service is running; IPC request failed".into(),
            Self::Stopping => "Windows service is stopping".into(),
            Self::Paused => "Windows service is paused".into(),
            Self::Other(state) => format!("Windows service state is {state}"),
            Self::Unknown(error) => format!("Unable to query Windows service: {error}"),
        }
    }

    pub fn connection_diagnostic(&self, ipc_error: &str) -> String {
        let advice = match self {
            Self::NotInstalled => {
                "Repair the MSI installation, or use launcher.exe for portable mode."
            }
            Self::Stopped { .. } => {
                "Start UltraSearchService in Services, or run 'sc.exe start UltraSearchService' as administrator."
            }
            Self::Starting { .. } => {
                "The UI will retry while the service creates its search endpoint."
            }
            Self::Running => {
                "The service is registered and running. Check the pipe error and service logs."
            }
            _ => "Check 'sc.exe query UltraSearchService' and the service logs.",
        };
        format!(
            "{}. {advice} IPC: {ipc_error}. Startup log: %PROGRAMDATA%\\UltraSearch\\log\\startup.log",
            self.summary()
        )
    }
}

#[cfg(not(windows))]
pub fn query_installed_service() -> InstalledServiceState {
    InstalledServiceState::Unknown(
        "Windows Service Control Manager is unavailable on this OS".into(),
    )
}

#[cfg(windows)]
pub fn query_installed_service() -> InstalledServiceState {
    use windows::Win32::Security::SC_HANDLE;
    use windows::Win32::System::Services::{
        CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus, SC_MANAGER_CONNECT,
        SERVICE_CONTINUE_PENDING, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_QUERY_STATUS,
        SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STOP_PENDING,
        SERVICE_STOPPED,
    };
    use windows::core::{PCWSTR, w};

    struct ServiceHandle(SC_HANDLE);
    impl Drop for ServiceHandle {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns a successful SCM handle.
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }

    // SAFETY: null machine/database names select the local SCM. Requested
    // permissions are read-only; guards close both successful handles.
    let manager =
        match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
            Ok(handle) => ServiceHandle(handle),
            Err(error) => return InstalledServiceState::Unknown(error.to_string()),
        };
    let service =
        match unsafe { OpenServiceW(manager.0, w!("UltraSearchService"), SERVICE_QUERY_STATUS) } {
            Ok(handle) => ServiceHandle(handle),
            Err(error) if error.code().0 as u32 == 0x8007_0424 => {
                // HRESULT_FROM_WIN32(ERROR_SERVICE_DOES_NOT_EXIST = 1060).
                return InstalledServiceState::NotInstalled;
            }
            Err(error) => return InstalledServiceState::Unknown(error.to_string()),
        };
    let mut status = SERVICE_STATUS::default();
    // SAFETY: service is live and status points to a correctly sized structure.
    if let Err(error) = unsafe { QueryServiceStatus(service.0, &mut status) } {
        return InstalledServiceState::Unknown(error.to_string());
    }
    match status.dwCurrentState {
        SERVICE_STOPPED => InstalledServiceState::Stopped {
            win32_exit_code: status.dwWin32ExitCode,
            service_exit_code: status.dwServiceSpecificExitCode,
        },
        SERVICE_START_PENDING | SERVICE_CONTINUE_PENDING => InstalledServiceState::Starting {
            checkpoint: status.dwCheckPoint,
            wait_hint_ms: status.dwWaitHint,
        },
        SERVICE_RUNNING => InstalledServiceState::Running,
        SERVICE_STOP_PENDING => InstalledServiceState::Stopping,
        SERVICE_PAUSED | SERVICE_PAUSE_PENDING => InstalledServiceState::Paused,
        state => InstalledServiceState::Other(state.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absent_registration_permits_a_portable_index_writer() {
        assert!(InstalledServiceState::NotInstalled.allows_console_fallback());
        for state in [
            InstalledServiceState::Stopped {
                win32_exit_code: 5,
                service_exit_code: 0,
            },
            InstalledServiceState::Starting {
                checkpoint: 2,
                wait_hint_ms: 30_000,
            },
            InstalledServiceState::Running,
            InstalledServiceState::Stopping,
            InstalledServiceState::Paused,
            InstalledServiceState::Other(99),
            InstalledServiceState::Unknown("access denied".into()),
        ] {
            assert!(!state.allows_console_fallback(), "{state:?}");
        }
    }

    #[test]
    fn diagnostics_separate_stopped_service_from_running_service_with_failed_pipe() {
        let stopped = InstalledServiceState::Stopped {
            win32_exit_code: 5,
            service_exit_code: 0,
        }
        .connection_diagnostic("pipe not found (os error 2)");
        assert!(stopped.contains("Windows service is stopped"));
        assert!(stopped.contains("Windows exit 5"));
        assert!(stopped.contains("sc.exe start UltraSearchService"));
        let running =
            InstalledServiceState::Running.connection_diagnostic("access denied (os error 5)");
        assert!(running.contains("running; IPC request failed"));
        assert!(running.contains("access denied (os error 5)"));
        assert!(running.contains("startup.log"));
        assert!(!running.contains("sc.exe start"));
    }
}
