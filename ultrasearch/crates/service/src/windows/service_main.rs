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
        if matches!(self.state, ServiceState::StopPending | ServiceState::Stopped) {
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
        if matches!(self.state, ServiceState::StopPending | ServiceState::Stopped) {
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
                let cfg = core_types::config::load_or_create_config(None)?;
                let cfg_path = core_types::config::default_config_path();
                crate::ensure_config_acl_writable(&cfg_path);
                run_app_with_startup(
                    &cfg,
                    shutdown_rx,
                    BootstrapOptions::default(),
                    |stage| app_reporter.lock().complete(stage),
                )
            }));
            let _ = done_tx.send(outcome);
        });
    let _worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            reporter.lock().finish(1)?;
            return Err(error).context("cannot start service application thread");
        }
    };

    let mut startup_timed_out = false;
    let exit_code = loop {
        // Check absolute deadlines even when controls arrive repeatedly. Never
        // report fictitious initialization progress from this polling loop.
        {
            let mut reporter = reporter.lock();
            let now = Instant::now();
            if reporter.lifecycle.start_expired(now) {
                startup_timed_out = true;
                eprintln!("UltraSearch initialization exceeded {START_GRACE:?}; stopping");
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
                break TIMEOUT_EXIT_CODE;
            }
        }
        match done_rx.recv_timeout(POLL_INTERVAL) {
            Ok(outcome) => {
                let reporter = reporter.lock();
                let code = exit_code_for(outcome, reporter.lifecycle.stop_started.is_some());
                break if startup_timed_out {
                    TIMEOUT_EXIT_CODE
                } else if reporter.failed {
                    1
                } else {
                    code
                };
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("UltraSearch application vanished without reporting");
                break 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    };

    // This is the only terminal report. Late startup callbacks and repeated
    // controls see Stopped and cannot publish again on the closed SCM context.
    reporter.lock().finish(exit_code)?;
    Ok(())
}

define_windows_service!(ffi_service_main, my_service_main);

fn my_service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run_service() {
        eprintln!("Service failed: {error:#}");
    }
}

/// Connect promptly to SCM; bootstrap happens only after handler registration.
pub fn launch() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|error| error.into())
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
                assert!(status.controls_accepted.contains(
                    ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
                ));
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
                assert!(state
                    .complete(StartupStage::EndpointReady, now)
                    .unwrap()
                    .is_none());
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
            assert!(state
                .complete(StartupStage::EndpointReady, stopped_at)
                .unwrap()
                .is_none());
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
            .complete(StartupStage::ConfigurationBound, start + Duration::from_secs(60))
            .unwrap();
        state
            .complete(StartupStage::IndexesReady, start + Duration::from_secs(119))
            .unwrap();
        assert!(state
            .complete(StartupStage::EndpointReady, start + START_GRACE)
            .is_err());
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
                exit_code_for(Ok(Err(anyhow::anyhow!("fixture startup failure"))), requested),
                1
            );
            assert_eq!(
                exit_code_for(Err(Box::new("fixture panic")), requested),
                1
            );
        }
    }
}
