use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPriority {
    Normal = 1,
    BelowNormal = 2,
    Idle = 3,
}

static LAST_PRIORITY: AtomicU8 = AtomicU8::new(0);

/// Return the last process priority requested in this process.
pub fn last_process_priority() -> Option<ProcessPriority> {
    match LAST_PRIORITY.load(Ordering::Relaxed) {
        1 => Some(ProcessPriority::Normal),
        2 => Some(ProcessPriority::BelowNormal),
        3 => Some(ProcessPriority::Idle),
        _ => None,
    }
}

/// Set process priority on Windows; records the requested priority on all platforms.
pub fn set_process_priority(priority: ProcessPriority) {
    LAST_PRIORITY.store(priority as u8, Ordering::Relaxed);

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
}

/// Ensure the interactive service host runs at Normal priority without background
/// thread I/O throttling so IPC and search queries remain responsive under load.
pub fn apply_service_priorities() {
    set_process_priority(ProcessPriority::Normal);
}

/// Apply CPU + I/O background-friendly priorities (intended for background worker processes).
pub fn apply_background_priorities() {
    set_process_priority(ProcessPriority::Idle);
    #[cfg(target_os = "windows")]
    {
        use tracing::warn;
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
        };
        unsafe {
            if let Err(e) = SetThreadPriority(
                GetCurrentThread(),
                windows::Win32::System::Threading::THREAD_PRIORITY(THREAD_MODE_BACKGROUND_BEGIN.0),
            ) {
                warn!("Failed to set background thread priority: {e:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_priority_uses_normal_and_background_uses_idle() {
        apply_service_priorities();
        assert_eq!(last_process_priority(), Some(ProcessPriority::Normal));

        apply_background_priorities();
        assert_eq!(last_process_priority(), Some(ProcessPriority::Idle));

        apply_service_priorities();
        assert_eq!(last_process_priority(), Some(ProcessPriority::Normal));
    }
}
