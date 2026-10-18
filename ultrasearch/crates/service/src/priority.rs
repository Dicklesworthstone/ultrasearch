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

/// Keep the resident query/journal service at normal scheduling priority.
/// Extraction workers apply their own idle CPU and background I/O policy.
pub fn apply_service_priorities() -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use anyhow::Context;
        use windows::Win32::Foundation::ERROR_THREAD_MODE_NOT_BACKGROUND;
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetCurrentThread, NORMAL_PRIORITY_CLASS, SetPriorityClass,
            SetThreadPriority, THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY,
            THREAD_PRIORITY_NORMAL,
        };
        use windows::core::HRESULT;

        // A same-process restart may reuse a thread configured by the old
        // bootstrap. End resource background mode before restoring CPU values;
        // setting CPU priority alone does not undo background I/O scheduling.
        // SAFETY: pseudo-handles refer only to our own process/current thread.
        unsafe {
            match SetThreadPriority(
                GetCurrentThread(),
                THREAD_PRIORITY(THREAD_MODE_BACKGROUND_END.0),
            ) {
                Err(error)
                    if error.code() == HRESULT::from_win32(ERROR_THREAD_MODE_NOT_BACKGROUND.0) => {}
                result => result.context("cannot end service thread background mode")?,
            }
            SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS)
                .context("cannot restore normal service process priority")?;
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL)
                .context("cannot restore normal service thread priority")?;
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use anyhow::{Result, ensure};
    use std::io::Write;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::ERROR_THREAD_MODE_NOT_BACKGROUND;
    use windows::Win32::System::Threading::{
        BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess, GetCurrentThread, GetPriorityClass,
        IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, SetPriorityClass, SetThreadPriority,
        THREAD_MODE_BACKGROUND_BEGIN, THREAD_MODE_BACKGROUND_END, THREAD_PRIORITY,
    };
    use windows::core::HRESULT;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn resident_priority_is_normal_and_background_exit_is_idempotent() -> Result<()> {
        const CHILD_MARKER: &str = "ULTRASEARCH_SERVICE_PRIORITY_TEST_CHILD";
        // Change priorities only in an isolated copy of this test executable,
        // never in the parent running unrelated Windows tests in parallel.
        if let Some(proof_path) = std::env::var_os(CHILD_MARKER) {
            for class in [IDLE_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS] {
                // SAFETY: the fixture lowers only its own process/thread.
                unsafe {
                    SetPriorityClass(GetCurrentProcess(), class)?;
                    SetThreadPriority(
                        GetCurrentThread(),
                        THREAD_PRIORITY(THREAD_MODE_BACKGROUND_BEGIN.0),
                    )?;
                }
                apply_service_priorities()?;
                apply_service_priorities()?;
                // SAFETY: query our own process and verify our current thread
                // is already outside background mode, rather than assuming it.
                unsafe {
                    ensure!(
                        GetPriorityClass(GetCurrentProcess()) == NORMAL_PRIORITY_CLASS.0,
                        "resident service remained below normal priority"
                    );
                    let error = SetThreadPriority(
                        GetCurrentThread(),
                        THREAD_PRIORITY(THREAD_MODE_BACKGROUND_END.0),
                    )
                    .expect_err("service thread must have already left background mode");
                    ensure!(
                        error.code() == HRESULT::from_win32(ERROR_THREAD_MODE_NOT_BACKGROUND.0),
                        "unexpected native background-mode status: {error}"
                    );
                }
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(proof_path)?
                .write_all(b"normal-priority-and-no-background-mode")?;
            return Ok(());
        }

        let evidence = tempfile::tempdir()?.keep();
        let proof_path = evidence.join("priority-verified.txt");
        let test_name = concat!(
            module_path!(),
            "::resident_priority_is_normal_and_background_exit_is_idempotent"
        );
        // libtest names omit the crate prefix supplied by module_path!().
        let test_name = test_name.split_once("::").unwrap().1;
        let mut child = ChildGuard(
            Command::new(std::env::current_exe()?)
                .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                .env(CHILD_MARKER, &proof_path)
                .spawn()?,
        );
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.0.try_wait()? {
                ensure!(status.success(), "native priority regression failed: {status}");
                ensure!(
                    std::fs::read(&proof_path)? == b"normal-priority-and-no-background-mode",
                    "child must actually execute the priority assertions, not match zero tests"
                );
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "native priority regression timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
