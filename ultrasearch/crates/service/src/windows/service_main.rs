//! Windows Service Control Manager (SCM) host for UltraSearch.
//!
//! Register before configuration IO. Report StartPending until bootstrap has
//! opened both index readers and bound its own IPC endpoint. Running describes
//! endpoint readiness, not completion of background MFT/journal reconciliation.
//! Status transitions share a lock: a late startup callback cannot undo STOP.
//! Startup and stop waits have absolute bounds; repeated controls/progress do
//! not extend them. Blocking application/native work is not forcibly unwound:
//! this OWN_PROCESS service returns after the stop grace and process exit owns
//! final teardown, as in the existing installed-service lifecycle.

use crate::bootstrap::{BootstrapOptions, StartupStage, run_app_with_startup};
use crate::logging::{flush_startup_diagnostics, record_startup_diagnostic};
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use std::ffi::OsString;
use std::ops::ControlFlow;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc::{self as std_mpsc, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
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

/// Must match ServiceInstall/@Name in WiX and the management subcommands.
pub const SERVICE_NAME: &str = "UltraSearchService";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const START_GRACE: Duration = Duration::from_secs(120);
const STOP_GRACE: Duration = Duration::from_secs(20);
const STOP_WAIT_HINT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
const DIAGNOSTIC_FLUSH_GRACE: Duration = Duration::from_millis(500);
// ERROR_SERVICE_REQUEST_TIMEOUT. A forced stop is not a clean application exit.
const TIMEOUT_EXIT_CODE: u32 = 1053;

type AppOutcome = std::thread::Result<Result<()>>;

/// The same state reducer is used by the control handler, bootstrap callbacks,
/// and host timeout/exit path. Only actual stage completion advances checkpoint.
struct Lifecycle {
    state: ServiceState,
    started: Instant,
    stop_started: Option<Instant>,
    stage: Option<StartupStage>,
    checkpoint: u32,
}

impl Lifecycle {
    fn new(now: Instant) -> Self {
        Self {
            state: ServiceState::StartPending,
            started: now,
            stop_started: None,
            stage: None,
            checkpoint: 1,
        }
    }

    fn snapshot(&self, exit_code: u32) -> ServiceStatus {
        let running = self.state == ServiceState::Running;
        ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: self.state,
            controls_accepted: if running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: self.checkpoint,
            wait_hint: match self.state {
                ServiceState::StartPending => START_GRACE,
                ServiceState::StopPending => STOP_WAIT_HINT,
                _ => Duration::ZERO,
            },
            process_id: None,
        }
    }

    fn complete(&mut self, stage: StartupStage, now: Instant) -> Result<Option<ServiceStatus>> {
        if matches!(
            self.state,
            ServiceState::StopPending | ServiceState::Stopped
        ) {
            return Ok(None);
        }
        ensure!(
            self.state == ServiceState::StartPending,
            "startup already completed"
        );
        ensure!(
            !self.start_expired(now),
            "service initialization exceeded its deadline"
        );
        let expected = match self.stage {
            None => StartupStage::RuntimeReady,
            Some(StartupStage::RuntimeReady) => StartupStage::ConfigurationBound,
            Some(StartupStage::ConfigurationBound) => StartupStage::IndexesReady,
            Some(StartupStage::IndexesReady) => StartupStage::EndpointReady,
            Some(StartupStage::EndpointReady) => anyhow::bail!("duplicate endpoint readiness"),
        };
        ensure!(
            stage == expected,
            "unexpected startup stage {stage:?}; expected {expected:?}"
        );
        self.stage = Some(stage);
        if stage == StartupStage::EndpointReady {
            self.state = ServiceState::Running;
            self.checkpoint = 0;
        } else {
            self.checkpoint += 1; // Exactly three progress steps, never timer ticks.
        }
        Ok(Some(self.snapshot(0)))
    }

    fn stop(&mut self, now: Instant) -> Option<ServiceStatus> {
        if matches!(
            self.state,
            ServiceState::StopPending | ServiceState::Stopped
        ) {
            return None;
        }
        self.state = ServiceState::StopPending;
        self.stop_started = Some(now);
        self.checkpoint = 1;
        Some(self.snapshot(0))
    }

    fn finish(&mut self, exit_code: u32) -> Option<ServiceStatus> {
        if self.state == ServiceState::Stopped {
            return None;
        }
        self.state = ServiceState::Stopped;
        self.checkpoint = 0;
        Some(self.snapshot(exit_code))
    }

    fn start_expired(&self, now: Instant) -> bool {
        self.state == ServiceState::StartPending
            && now.saturating_duration_since(self.started) >= START_GRACE
    }

    fn stop_expired(&self, now: Instant) -> bool {
        self.stop_started
            .is_some_and(|at| now.saturating_duration_since(at) >= STOP_GRACE)
    }

    fn diagnostic_flush_budget(&self, now: Instant) -> Duration {
        let remaining = if let Some(started) = self.stop_started {
            STOP_GRACE.saturating_sub(now.saturating_duration_since(started))
        } else if self.state == ServiceState::StartPending {
            START_GRACE.saturating_sub(now.saturating_duration_since(self.started))
        } else {
            DIAGNOSTIC_FLUSH_GRACE
        };
        remaining.min(DIAGNOSTIC_FLUSH_GRACE)
    }
}

struct Reporter {
    lifecycle: Lifecycle,
    handle: Option<ServiceStatusHandle>,
    failed: bool,
}

impl Reporter {
    fn publish(&mut self, status: Option<ServiceStatus>) -> Result<()> {
        if let (Some(handle), Some(status)) = (&self.handle, status)
            && let Err(error) = handle.set_service_status(status)
        {
            self.failed = true;
            return Err(error).context("cannot publish service status");
        }
        Ok(())
    }

    fn stop(&mut self, now: Instant) -> Result<()> {
        let status = self.lifecycle.stop(now);
        self.publish(status)
    }

    fn complete(&mut self, stage: StartupStage) -> Result<ControlFlow<()>> {
        let Some(status) = self.lifecycle.complete(stage, Instant::now())? else {
            return Ok(ControlFlow::Break(()));
        };
        self.publish(Some(status))?;
        Ok(ControlFlow::Continue(()))
    }

    fn finish(&mut self, exit_code: u32) -> Result<()> {
        let status = self.lifecycle.finish(exit_code);
        self.publish(status)
    }
}

fn exit_code_for(outcome: AppOutcome, stop_requested: bool) -> u32 {
    match outcome {
        Ok(Ok(())) if stop_requested => 0,
        Ok(Ok(())) => {
            eprintln!("UltraSearch application returned without a stop request");
            1
        }
        Ok(Err(error)) => {
            eprintln!("UltraSearch service exited with error: {error:#}");
            1
        }
        Err(_) => {
            eprintln!("UltraSearch service worker panicked");
            1
        }
    }
}

fn outcome_failure(outcome: &AppOutcome, stop_requested: bool) -> Option<String> {
    match outcome {
        Ok(Ok(())) if stop_requested => None,
        Ok(Ok(())) => Some("application returned without a stop request".into()),
        Ok(Err(error)) => Some(format!("{error:#}")),
        Err(payload) => Some(format!(
            "application worker panicked: {}",
            payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic payload")
        )),
    }
}

pub fn run_service() -> Result<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
    let reporter = Arc::new(Mutex::new(Reporter {
        lifecycle: Lifecycle::new(Instant::now()),
        handle: None,
        failed: false,
    }));
    let handler_reporter = Arc::clone(&reporter);
    let handler_shutdown = shutdown_tx.clone();
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                // Latch and report under the same lock used by readiness. Do
                // not let a slow bootstrap overwrite StopPending with Running.
                if let Err(error) = handler_reporter.lock().stop(Instant::now()) {
                    eprintln!("Cannot report service stop: {error:#}");
                }
                let _ = handler_shutdown.try_send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Nothing that reads configuration or opens indices runs before registration.
    let handle = service_control_handler::register(SERVICE_NAME, event_handler)?;
    {
        let mut reporter = reporter.lock();
        reporter.handle = Some(handle);
        let initial = reporter.lifecycle.snapshot(0);
        reporter.publish(Some(initial))?;
        if reporter.lifecycle.stop_started.is_some() {
            return reporter.finish(0);
        }
    }

    let (done_tx, done_rx) = std_mpsc::channel::<AppOutcome>();
    let app_reporter = Arc::clone(&reporter);
    let worker = thread::Builder::new()
        .name("ultrasearch-app".into())
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                dotenvy::dotenv().ok();
                let cfg_path = core_types::config::default_config_path();
                record_startup_diagnostic("configuration", "SCM registered; loading configuration");
                let cfg = core_types::config::load_or_create_config(Some(&cfg_path)).with_context(
                    || format!("configuration loading failed: {}", cfg_path.display()),
                )?;
                crate::ensure_config_acl_writable(&cfg_path);
                run_app_with_startup(&cfg, shutdown_rx, BootstrapOptions::default(), |stage| {
                    let progress = app_reporter.lock().complete(stage)?;
                    if progress.is_continue() {
                        record_startup_diagnostic("readiness", &format!("completed {stage:?}"));
                    }
                    Ok(progress)
                })
            }));
            let _ = done_tx.send(outcome);
        });
    let _worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            eprintln!("Cannot start service application thread: {error}");
            record_startup_diagnostic(
                "failed",
                &format!("cannot start application thread: {error}"),
            );
            let flush_budget = reporter
                .lock()
                .lifecycle
                .diagnostic_flush_budget(Instant::now());
            flush_startup_diagnostics(flush_budget);
            reporter.lock().finish(1)?;
            return Ok(());
        }
    };

    let mut startup_timed_out = false;
    let exit_code = loop {
        // Check absolute deadlines even when controls arrive repeatedly. Never
        // report fictitious initialization progress from this polling loop.
        let mut timeout_diagnostic = None;
        let mut stop_timed_out = false;
        {
            let mut reporter = reporter.lock();
            let now = Instant::now();
            if reporter.lifecycle.start_expired(now) {
                startup_timed_out = true;
                eprintln!("UltraSearch initialization exceeded {START_GRACE:?}; stopping");
                timeout_diagnostic = Some(format!(
                    "initialization exceeded {START_GRACE:?}; last completed stage: {:?}",
                    reporter.lifecycle.stage
                ));
                if let Err(error) = reporter.stop(now) {
                    eprintln!("Cannot report timed-out initialization: {error:#}");
                }
                let _ = shutdown_tx.try_send(());
            } else if reporter.failed && reporter.lifecycle.stop_started.is_none() {
                let _ = reporter.stop(now);
                let _ = shutdown_tx.try_send(());
            }
            if reporter.lifecycle.stop_expired(now) {
                eprintln!("UltraSearch application did not stop within {STOP_GRACE:?}");
                timeout_diagnostic =
                    Some(format!("application did not stop within {STOP_GRACE:?}"));
                stop_timed_out = true;
            }
        }
        if let Some(message) = timeout_diagnostic {
            record_startup_diagnostic("timeout", &message);
        }
        if stop_timed_out {
            break TIMEOUT_EXIT_CODE;
        }
        match done_rx.recv_timeout(POLL_INTERVAL) {
            Ok(outcome) => {
                let (stop_requested, stage, reporting_failed) = {
                    let reporter = reporter.lock();
                    (
                        reporter.lifecycle.stop_started.is_some(),
                        reporter.lifecycle.stage,
                        reporter.failed,
                    )
                };
                if let Some(failure) = outcome_failure(&outcome, stop_requested) {
                    record_startup_diagnostic(
                        "failed",
                        &format!("last completed stage: {stage:?}; {failure}"),
                    );
                }
                let code = exit_code_for(outcome, stop_requested);
                break if startup_timed_out {
                    TIMEOUT_EXIT_CODE
                } else if reporting_failed {
                    1
                } else {
                    code
                };
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("UltraSearch application vanished without reporting");
                record_startup_diagnostic("failed", "application vanished without reporting");
                break 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    };

    // This is the only terminal report. Late startup callbacks and repeated
    // controls see Stopped and cannot publish again on the closed SCM context.
    record_startup_diagnostic("stopped", &format!("SCM exit code {exit_code}"));
    // Normal failures (especially invalid config before tracing initializes)
    // should reach disk before STOPPED: Windows may terminate the process as
    // soon as that status is published. This wait cannot extend the absolute
    // startup/stop grace, and forced deadline exits do not wait at all.
    if exit_code != TIMEOUT_EXIT_CODE {
        let flush_budget = reporter
            .lock()
            .lifecycle
            .diagnostic_flush_budget(Instant::now());
        flush_startup_diagnostics(flush_budget);
    }
    let final_status = reporter.lock().finish(exit_code);
    if let Err(error) = final_status {
        // A failed terminal status call must not re-enter the wrapper's normal
        // failure flush, especially after the absolute stop deadline (1053).
        // Enqueue evidence only; stderr/file IO belong to the detached writer.
        record_startup_diagnostic(
            "terminal status failure",
            &format!("cannot publish final SCM status (exit {exit_code}): {error:#}"),
        );
    }
    Ok(())
}

define_windows_service!(ffi_service_main, my_service_main);

fn my_service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        eprintln!("Service failed: {error:#}");
        record_startup_diagnostic("host failure", &format!("{error:#}"));
        flush_startup_diagnostics(DIAGNOSTIC_FLUSH_GRACE);
    }
}

/// Connect promptly to SCM; bootstrap happens only after handler registration.
pub fn launch() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(dispatcher_error)
}

fn dispatcher_error(error: windows_service::Error) -> anyhow::Error {
    let manual_launch = matches!(
        &error,
        windows_service::Error::Winapi(error) if error.raw_os_error() == Some(1063)
    );
    let error = anyhow::Error::new(error);
    if manual_launch {
        error.context(
            "This executable was launched outside the Windows Service Control Manager (error 1063). \
             Start the installed service with `sc.exe start UltraSearchService` from an elevated terminal. \
             For interactive diagnostics, first stop the installed service and run this executable with `--console`. \
             This manual-launch error does not report whether the installed service is healthy.",
        )
    } else {
        error.context("cannot connect UltraSearchService to the Windows Service Control Manager")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGES: [StartupStage; 4] = [
        StartupStage::RuntimeReady,
        StartupStage::ConfigurationBound,
        StartupStage::IndexesReady,
        StartupStage::EndpointReady,
    ];

    #[test]
    fn manual_launch_error_preserves_1063_and_explains_the_two_launch_modes() {
        let error = dispatcher_error(windows_service::Error::Winapi(
            std::io::Error::from_raw_os_error(1063),
        ));
        let message = format!("{error:#}");
        assert!(message.contains("sc.exe start UltraSearchService"));
        assert!(message.contains("--console"));
        assert!(message.contains("does not report whether the installed service is healthy"));
        assert!(error.chain().any(|source| {
            source
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(1063))
        }));
        let other = dispatcher_error(windows_service::Error::Winapi(
            std::io::Error::from_raw_os_error(5),
        ));
        assert!(!other.to_string().contains("manual-launch"));
        assert!(!other.to_string().contains("--console"));
    }

    #[test]
    fn diagnostics_retain_configuration_causes_and_unexpected_shutdowns() {
        let failed = Ok(Err(anyhow::anyhow!("invalid TOML at line 3").context(
            "configuration loading failed: C:\\ProgramData\\UltraSearch\\config\\config.toml",
        )));
        let message = outcome_failure(&failed, false).unwrap();
        assert!(message.contains("configuration loading failed"));
        assert!(message.contains("invalid TOML at line 3"));
        assert!(outcome_failure(&Ok(Ok(())), true).is_none());
        assert!(outcome_failure(&Ok(Ok(())), false).is_some());
        assert_eq!(
            outcome_failure(&Err(Box::new("fixture panic")), false).unwrap(),
            "application worker panicked: fixture panic"
        );
    }

    #[test]
    fn diagnostic_flush_cannot_extend_startup_or_stop_deadlines() {
        let start = Instant::now();
        let mut state = Lifecycle::new(start);
        assert_eq!(state.diagnostic_flush_budget(start), DIAGNOSTIC_FLUSH_GRACE);
        assert_eq!(
            state.diagnostic_flush_budget(start + START_GRACE - Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        assert_eq!(
            state.diagnostic_flush_budget(start + START_GRACE),
            Duration::ZERO
        );
        let stop = start + Duration::from_secs(1);
        state.stop(stop);
        assert_eq!(state.diagnostic_flush_budget(stop), DIAGNOSTIC_FLUSH_GRACE);
        assert_eq!(
            state.diagnostic_flush_budget(stop + STOP_GRACE - Duration::from_millis(10)),
            Duration::from_millis(10)
        );
        assert_eq!(
            state.diagnostic_flush_budget(stop + STOP_GRACE),
            Duration::ZERO
        );
    }

    #[test]
    fn running_requires_all_completed_stages_and_has_valid_status_fields() {
        let start = Instant::now();
        let mut state = Lifecycle::new(start);
        let initial = state.snapshot(0);
        assert_eq!(initial.current_state, ServiceState::StartPending);
        assert_eq!(initial.checkpoint, 1);
        assert!(initial.controls_accepted.is_empty());
        for (index, stage) in STAGES.into_iter().enumerate() {
            let status = state
                .complete(stage, start + Duration::from_secs(index as u64))
                .unwrap()
                .unwrap();
            if stage == StartupStage::EndpointReady {
                assert_eq!(status.current_state, ServiceState::Running);
                assert_eq!(status.checkpoint, 0);
                assert_eq!(status.wait_hint, Duration::ZERO);
                assert!(
                    status
                        .controls_accepted
                        .contains(ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN)
                );
            } else {
                assert_eq!(status.current_state, ServiceState::StartPending);
                assert_eq!(status.checkpoint, index as u32 + 2);
                assert!(status.controls_accepted.is_empty());
            }
        }
        assert!(state.complete(StartupStage::EndpointReady, start).is_err());
    }

    #[test]
    fn stop_is_terminal_for_startup_and_repeated_controls_do_not_extend_grace() {
        for completed in 0..=STAGES.len() {
            let start = Instant::now();
            let mut state = Lifecycle::new(start);
            for stage in STAGES.iter().take(completed) {
                state.complete(*stage, start).unwrap();
            }
            let stopped_at = start + Duration::from_secs(10);
            let status = state.stop(stopped_at).unwrap();
            assert_eq!(status.current_state, ServiceState::StopPending);
            assert!(status.controls_accepted.is_empty());
            for second in 1..=20 {
                let now = stopped_at + Duration::from_secs(second);
                assert!(state.stop(now).is_none());
                assert!(
                    state
                        .complete(StartupStage::EndpointReady, now)
                        .unwrap()
                        .is_none()
                );
                assert_eq!(state.stop_started, Some(stopped_at));
                assert_eq!(state.stop_expired(now), second >= 20);
            }
            let status = state.finish(TIMEOUT_EXIT_CODE).unwrap();
            assert_eq!(status.current_state, ServiceState::Stopped);
            assert_eq!(status.exit_code, ServiceExitCode::Win32(TIMEOUT_EXIT_CODE));
            assert_eq!(status.checkpoint, 0);
            assert_eq!(status.wait_hint, Duration::ZERO);
            assert!(state.finish(0).is_none());
            assert!(state.stop(stopped_at).is_none());
            assert!(
                state
                    .complete(StartupStage::EndpointReady, stopped_at)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn invalid_or_late_progress_never_inflates_checkpoints_or_reports_running() {
        let start = Instant::now();
        let mut state = Lifecycle::new(start);
        for stage in STAGES.into_iter().skip(1) {
            assert!(state.complete(stage, start).is_err());
            assert_eq!(state.checkpoint, 1);
            assert_eq!(state.stage, None);
        }
        state.complete(StartupStage::RuntimeReady, start).unwrap();
        assert!(state.complete(StartupStage::RuntimeReady, start).is_err());
        assert_eq!(state.checkpoint, 2);
        state
            .complete(
                StartupStage::ConfigurationBound,
                start + Duration::from_secs(60),
            )
            .unwrap();
        state
            .complete(StartupStage::IndexesReady, start + Duration::from_secs(119))
            .unwrap();
        assert!(
            state
                .complete(StartupStage::EndpointReady, start + START_GRACE)
                .is_err()
        );
        assert_eq!(state.state, ServiceState::StartPending);
        assert!(state.start_expired(start + START_GRACE));
        assert_eq!(state.started, start);
    }

    #[test]
    fn application_errors_and_unrequested_completion_are_not_clean_stops() {
        assert_eq!(exit_code_for(Ok(Ok(())), false), 1);
        assert_eq!(exit_code_for(Ok(Ok(())), true), 0);
        for requested in [false, true] {
            assert_eq!(
                exit_code_for(
                    Ok(Err(anyhow::anyhow!("fixture startup failure"))),
                    requested
                ),
                1
            );
            assert_eq!(exit_code_for(Err(Box::new("fixture panic")), requested), 1);
        }
    }
}
