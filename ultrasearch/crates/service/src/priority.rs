#[derive(Debug, Clone, Copy)]
pub enum ProcessPriority {
    Normal,
    BelowNormal,
    Idle,
}

/// Set process priority on Windows; no-op on other platforms for now.
pub fn set_process_priority(priority: ProcessPriority) {
    #[cfg(target_os = "windows")]
    {
        use tracing::warn;
        use windows::Win32::System::Threading::{
            BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, IDLE_PRIORITY_CLASS,
            NORMAL_PRIORITY_CLASS, SetPriorityClass,
        };

        let class = match priority {
            ProcessPriority::Normal => NORMAL_PRIORITY_CLASS,
            ProcessPriority::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
        };

        unsafe {
            if let Err(e) = SetPriorityClass(GetCurrentProcess(), class) {
                warn!("Failed to set process priority: {e:?}");
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    let _ = priority;
}
