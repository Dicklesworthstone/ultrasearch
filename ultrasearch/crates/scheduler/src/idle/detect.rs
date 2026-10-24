use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    Active,
    WarmIdle,
    DeepIdle,
}

#[derive(Debug, Clone, Copy)]
pub struct IdleSample {
    /// Classified state at the time of sampling.
    pub state: IdleState,
    /// Conservative idle time across successfully sampled interactive sessions.
    pub idle_for: Duration,
    /// Time elapsed since we last transitioned into this state.
    pub since_state_change: Duration,
}

/// Tracks activity across interactive Windows sessions, including from Session 0.
///
/// The tracker keeps a lightweight state machine so callers can react to
/// transitions (e.g., schedule heavier jobs only after sustaining DeepIdle).
pub struct IdleTracker<F = fn() -> Option<u64>> {
    warm_idle: Duration,
    deep_idle: Duration,
    reader: F,
    last_state: IdleState,
    last_transition: Instant,
}

impl IdleTracker {
    /// Create a tracker that uses the OS idle timer.
    pub fn new(warm_idle: Duration, deep_idle: Duration) -> Self {
        Self::with_reader(warm_idle, deep_idle, idle_elapsed_ms)
    }
}

impl<F> IdleTracker<F>
where
    F: FnMut() -> Option<u64>,
{
    /// Build a tracker with a custom idle-time reader (handy for tests).
    pub fn with_reader(warm_idle: Duration, deep_idle: Duration, reader: F) -> Self {
        assert!(
            deep_idle >= warm_idle,
            "deep_idle must be greater than or equal to warm_idle",
        );

        Self {
            warm_idle,
            deep_idle,
            reader,
            last_state: IdleState::Active,
            last_transition: Instant::now(),
        }
    }

    /// Return the current `(warm_idle, deep_idle)` thresholds.
    pub fn thresholds(&self) -> (Duration, Duration) {
        (self.warm_idle, self.deep_idle)
    }

    /// Update policy without replacing the reader or discarding its observations.
    /// The next sample reclassifies against these thresholds. Transition time
    /// changes only when that sample actually changes the classified state.
    ///
    /// # Panics
    /// Panics before changing either threshold if `deep_idle < warm_idle`, as
    /// does the constructor. Live service configuration is validated on load.
    /// Callers that must tolerate an unordered pair clamp before calling.
    pub fn set_thresholds(&mut self, warm_idle: Duration, deep_idle: Duration) {
        assert!(
            deep_idle >= warm_idle,
            "deep_idle must be greater than or equal to warm_idle",
        );
        self.warm_idle = warm_idle;
        self.deep_idle = deep_idle;
    }

    /// Read the current idle state and update transition bookkeeping.
    pub fn sample(&mut self) -> IdleSample {
        let observed = (self.reader)().map(Duration::from_millis);
        let idle_for = observed.unwrap_or(Duration::ZERO);
        // Unknown input is not evidence of idleness, even with zero thresholds.
        let state = observed.map_or(IdleState::Active, |idle| {
            classify_idle(idle, self.warm_idle, self.deep_idle)
        });

        if state != self.last_state {
            self.last_state = state;
            self.last_transition = Instant::now();
        }

        IdleSample {
            state,
            idle_for,
            since_state_change: self.last_transition.elapsed(),
        }
    }
}

pub fn classify_idle(idle_for: Duration, warm_idle: Duration, deep_idle: Duration) -> IdleState {
    if idle_for >= deep_idle {
        IdleState::DeepIdle
    } else if idle_for >= warm_idle {
        IdleState::WarmIdle
    } else {
        IdleState::Active
    }
}

#[cfg(target_os = "windows")]
fn idle_elapsed_ms() -> Option<u64> {
    native::sample()
}

#[cfg(not(target_os = "windows"))]
fn idle_elapsed_ms() -> Option<u64> {
    // Non-Windows platforms: caller can still inject a reader in tests;
    // default fallback is "active".
    None
}

// These helpers also run in portable tests. No non-Windows production sensor
// is fabricated: unsupported platforms still return None above.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SessionEpoch {
    id: u32,
    logon: i64,
    connected: i64,
    input: i64,
}

#[cfg(any(windows, test))]
struct SessionObservation {
    sessions: Vec<SessionEpoch>,
    // None here means a successfully enumerated unattended machine, not an
    // unavailable sensor. The outer Result/Option carries collection failure.
    idle: Option<Duration>,
}

#[cfg(any(windows, test))]
impl SessionObservation {
    fn from_active(mut sessions: Vec<(SessionEpoch, Duration)>) -> Self {
        sessions.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        Self {
            idle: sessions.iter().map(|(_, idle)| *idle).min(),
            sessions: sessions.into_iter().map(|(epoch, _)| epoch).collect(),
        }
    }
}

#[cfg(any(windows, test))]
fn session_idle(current: i64, input: i64, logon: i64, connected: i64) -> Option<Duration> {
    // WTSInfoEx uses full-width UTC FILETIME units (100 ns since 1601), not
    // GetLastInputInfo's wrapping DWORD tick. Zero/negative input is unknown.
    if current <= 0 || input <= 0 || logon <= 0 || connected <= 0 {
        return None;
    }
    let recent = input.max(logon).max(connected);
    let ticks = u64::try_from(current.checked_sub(recent)?).ok()?;
    Some(Duration::from_millis(ticks / 10_000))
}

#[cfg(any(windows, test))]
const MAX_OBSERVATION_GAP: Duration = Duration::from_secs(5);

#[cfg(any(windows, test))]
#[derive(Default)]
struct SessionClock {
    previous: Option<(Vec<SessionEpoch>, Duration, Instant)>,
}

#[cfg(any(windows, test))]
impl SessionClock {
    fn observe(&mut self, observation: Option<SessionObservation>, now: Instant) -> Option<u64> {
        let Some(observation) = observation else {
            self.previous = None;
            return None;
        };
        // Never turn an incomplete set of per-session queries into unattended
        // time. Native collection constructs these two cases explicitly.
        if observation.sessions.is_empty() != observation.idle.is_none() {
            self.previous = None;
            return None;
        }
        let elapsed = self.previous.as_ref().and_then(|(sessions, idle, at)| {
            let elapsed = now.checked_duration_since(*at)?;
            (sessions == &observation.sessions && elapsed <= MAX_OBSERVATION_GAP)
                .then_some(idle.saturating_add(elapsed))
        });
        // UTC can jump forward. Accrue no more than monotonic elapsed time
        // between successful samples, and honor any more recent user input.
        // Startup, new input, session replacement/reconnect, or a sampling gap
        // begins at zero; a confirmed headless machine accrues idle normally.
        // Tracking input itself also handles a clock jump coinciding with input.
        let mut idle = elapsed.unwrap_or(Duration::ZERO);
        if let Some(observed) = observation.idle {
            idle = idle.min(observed);
        }
        self.previous = Some((observation.sessions, idle, now));
        Some(idle.as_millis().min(u128::from(u64::MAX)) as u64)
    }
}

#[cfg(windows)]
mod native {
    use super::*;
    use anyhow::{Context, Result, ensure};
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::RemoteDesktop::{
        WTS_SESSION_INFOW, WTSActive, WTSDisconnected, WTSEnumerateSessionsW, WTSFreeMemory,
        WTSINFOEXW, WTSIdle, WTSListen, WTSQuerySessionInformationW, WTSSessionInfoEx,
    };
    use windows::core::PWSTR;

    const MAX_SESSIONS: usize = 256;

    struct WtsBuffer(*mut c_void);

    impl Drop for WtsBuffer {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: each non-null allocation was returned by WTS and
                // is freed exactly once, after all borrowed data has been read.
                unsafe { WTSFreeMemory(self.0) };
            }
        }
    }

    fn sessions() -> Result<Vec<(u32, i32)>> {
        let mut entries: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0;
        // SAFETY: null server is the local machine. Version 1, reserved 0,
        // live output pointers, and WTS-owned memory retained by the guard.
        let result = unsafe { WTSEnumerateSessionsW(HANDLE(0), 0, 1, &mut entries, &mut count) };
        let _buffer = WtsBuffer(entries.cast());
        result.context("cannot enumerate interactive sessions")?;
        ensure!(
            count as usize <= MAX_SESSIONS,
            "session enumeration exceeds bound"
        );
        ensure!(
            count == 0 || !entries.is_null(),
            "missing session enumeration buffer"
        );
        let mut sessions = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            // SAFETY: successful enumeration returned count entries. Read only
            // scalar fields; never dereference names embedded in the response.
            let entry = unsafe { entries.add(index).read_unaligned() };
            if entry.SessionId != 0 {
                sessions.push((entry.SessionId, entry.State.0));
            }
        }
        sessions.sort_unstable();
        ensure!(
            sessions.windows(2).all(|pair| pair[0].0 != pair[1].0),
            "duplicate session identity"
        );
        Ok(sessions)
    }

    fn active_session(id: u32) -> Result<(SessionEpoch, Duration)> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0;
        // SAFETY: query a specific local session, not the calling service's
        // WTS_CURRENT_SESSION. The allocation lives until after the copy below.
        let result = unsafe {
            WTSQuerySessionInformationW(HANDLE(0), id, WTSSessionInfoEx, &mut buffer, &mut bytes)
        };
        let _buffer = WtsBuffer(buffer.0.cast());
        result.with_context(|| format!("cannot query input for session {id}"))?;
        ensure!(
            !buffer.0.is_null() && bytes as usize >= std::mem::size_of::<WTSINFOEXW>(),
            "truncated session information for {id}"
        );
        // SAFETY: byte count covers the complete structure. read_unaligned
        // avoids assuming a stronger alignment than the WTS byte-buffer API.
        let info = unsafe { buffer.0.cast::<WTSINFOEXW>().read_unaligned() };
        ensure!(info.Level == 1, "unsupported session information level");
        // SAFETY: Level 1 selects exactly this Copy union member.
        let info = unsafe { info.Data.WTSInfoExLevel1 };
        ensure!(
            info.SessionId == id && info.SessionState == WTSActive,
            "session identity/state changed during input query"
        );
        let idle = session_idle(
            info.CurrentTime,
            info.LastInputTime,
            info.LogonTime,
            info.ConnectTime,
        )
        .context("session input timestamp is unavailable or inconsistent")?;
        Ok((
            SessionEpoch {
                id,
                logon: info.LogonTime,
                connected: info.ConnectTime,
                input: info.LastInputTime,
            },
            idle,
        ))
    }

    pub(super) fn observation() -> Result<SessionObservation> {
        let started = Instant::now();
        let before = sessions()?;
        let mut active = Vec::new();
        for &(id, state) in &before {
            ensure!(
                started.elapsed() <= MAX_OBSERVATION_GAP,
                "session sample was too slow to trust"
            );
            if state == WTSActive.0 {
                active.push(active_session(id)?);
            } else {
                // Disconnected/waiting/listener sessions cannot receive user
                // input. Connecting, shadowing, reset and unknown states are
                // not proof of idleness; retry on the next scheduler turn.
                ensure!(
                    [WTSDisconnected.0, WTSIdle.0, WTSListen.0].contains(&state),
                    "session {id} is transitioning or has unknown state {state}"
                );
            }
        }
        ensure!(
            started.elapsed() <= MAX_OBSERVATION_GAP,
            "session sample was too slow to trust"
        );
        ensure!(
            sessions()? == before,
            "session set changed during input sampling"
        );
        ensure!(
            started.elapsed() <= MAX_OBSERVATION_GAP,
            "session sample was too slow to trust"
        );
        Ok(SessionObservation::from_active(active))
    }

    #[derive(Default)]
    struct Monitor {
        clock: SessionClock,
        unavailable: bool,
    }

    pub(super) fn sample() -> Option<u64> {
        static MONITOR: OnceLock<Mutex<Monitor>> = OnceLock::new();
        // Calls are bounded in number, not forcibly cancellable. Do not hold
        // monitor state while WTS performs potentially blocking local RPC.
        let observation = observation();
        let mut monitor = MONITOR
            .get_or_init(|| Mutex::new(Monitor::default()))
            .lock()
            .ok()?;
        match observation {
            Ok(observation) => {
                if monitor.unavailable {
                    tracing::info!("interactive-session idle sensing recovered");
                }
                monitor.unavailable = false;
                monitor.clock.observe(Some(observation), Instant::now())
            }
            Err(error) => {
                if !monitor.unavailable {
                    tracing::warn!(
                        %error,
                        "interactive-session idle sensing unavailable; treating as active"
                    );
                }
                monitor.unavailable = true;
                monitor.clock.observe(None, Instant::now())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[test]
    fn classify_respects_thresholds() {
        let warm = Duration::from_secs(15);
        let deep = Duration::from_secs(60);

        assert_eq!(
            classify_idle(Duration::from_secs(0), warm, deep),
            IdleState::Active
        );
        assert_eq!(
            classify_idle(Duration::from_secs(20), warm, deep),
            IdleState::WarmIdle
        );
        assert_eq!(
            classify_idle(Duration::from_secs(90), warm, deep),
            IdleState::DeepIdle
        );
    }

    #[test]
    fn tracker_updates_transition_time() {
        let mut values = VecDeque::from([0u64, 20_000, 70_000]);
        let mut tracker = IdleTracker::with_reader(
            Duration::from_secs(15),
            Duration::from_secs(60),
            move || values.pop_front(),
        );

        let first = tracker.sample();
        assert_eq!(first.state, IdleState::Active);

        let warm = tracker.sample();
        assert_eq!(warm.state, IdleState::WarmIdle);
        assert!(warm.since_state_change.as_millis() < 100);

        let deep = tracker.sample();
        assert_eq!(deep.state, IdleState::DeepIdle);
    }

    #[test]
    fn threshold_reload_reclassifies_without_resetting_reader_or_same_state_time() {
        let mut samples = VecDeque::from([Some(45_000), Some(45_000), Some(45_000), None]);
        let mut tracker = IdleTracker::with_reader(
            Duration::from_secs(15),
            Duration::from_secs(60),
            || samples.pop_front().expect("reload must not consume a sample"),
        );
        assert_eq!(tracker.sample().state, IdleState::WarmIdle);
        let unchanged_transition = Instant::now() - Duration::from_secs(10);
        tracker.last_transition = unchanged_transition;

        tracker.set_thresholds(Duration::from_secs(10), Duration::from_secs(90));
        assert_eq!(tracker.sample().state, IdleState::WarmIdle);
        assert_eq!(tracker.last_transition, unchanged_transition);

        tracker.set_thresholds(Duration::from_secs(10), Duration::from_secs(30));
        assert_eq!(tracker.sample().state, IdleState::DeepIdle);
        assert!(tracker.last_transition > unchanged_transition);

        tracker.set_thresholds(Duration::ZERO, Duration::ZERO);
        assert_eq!(tracker.sample().state, IdleState::Active);
    }

    #[test]
    fn invalid_threshold_reload_leaves_both_thresholds_and_reader_intact() {
        let mut tracker = IdleTracker::with_reader(
            Duration::from_secs(15),
            Duration::from_secs(60),
            || Some(45_000),
        );
        let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracker.set_thresholds(Duration::from_secs(90), Duration::from_secs(30));
        }));
        assert!(rejected.is_err());
        assert_eq!(tracker.warm_idle, Duration::from_secs(15));
        assert_eq!(tracker.deep_idle, Duration::from_secs(60));
        assert_eq!(tracker.sample().state, IdleState::WarmIdle);
    }

    fn observation(sessions: &[(u32, u64)]) -> SessionObservation {
        SessionObservation::from_active(
            sessions
                .iter()
                .map(|&(id, millis)| {
                    (
                        SessionEpoch {
                            id,
                            logon: 1,
                            connected: 1,
                            input: 1,
                        },
                        Duration::from_millis(millis),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn missing_input_never_permits_background_work_even_at_zero_thresholds() {
        for threshold in [Duration::ZERO, Duration::from_secs(60)] {
            let mut samples = VecDeque::from([Some(90_000), None, None, Some(90_000)]);
            let mut tracker = IdleTracker::with_reader(threshold, threshold, || {
                samples.pop_front().unwrap()
            });
            assert_eq!(tracker.sample().state, IdleState::DeepIdle);
            for _ in 0..2 {
                let sample = tracker.sample();
                assert_eq!(sample.state, IdleState::Active);
                assert_eq!(sample.idle_for, Duration::ZERO);
            }
            assert_eq!(tracker.sample().state, IdleState::DeepIdle);
        }
    }

    #[test]
    fn session_timestamps_are_full_width_and_reconnect_is_activity() {
        let now = 133_000_000_000_000_000i64;
        let seconds = 10_000_000;
        assert_eq!(
            session_idle(now, now - 2 * seconds, now - 100 * seconds, now - 90 * seconds),
            Some(Duration::from_secs(2))
        );
        // Full-width input timestamps are independent of uptime/DWORD wraps.
        for shift in [0, i64::from(u32::MAX) * 10_000, 100 * i64::from(u32::MAX) * 10_000] {
            assert_eq!(
                session_idle(now + shift, now + shift - seconds, now - seconds, now - seconds),
                Some(Duration::from_secs(1))
            );
        }
        assert_eq!(
            session_idle(now, now - 100 * seconds, now - 200 * seconds, now - seconds),
            Some(Duration::from_secs(1))
        );
        for (current, input, logon, connected) in [
            (now, 0, 1, 1), (0, 1, 1, 1), (now, 1, 0, 1),
            (now, 1, 1, 0), (now, now + 1, 1, 1), (now, 1, now + 1, 1),
            (now, 1, 1, now + 1), (i64::MAX, i64::MIN, 1, 1),
        ] {
            assert_eq!(session_idle(current, input, logon, connected), None);
        }
    }

    #[test]
    fn most_recent_interactive_session_controls_machine_idle() {
        let now = Instant::now();
        let mut clock = SessionClock::default();
        assert_eq!(clock.observe(Some(observation(&[(2, 500_000), (1, 500_000)])), now), Some(0));
        for second in 1..=65 {
            let sample = observation(&[(1, 500_000), (2, 500_000)]);
            assert_eq!(
                clock.observe(Some(sample), now + Duration::from_secs(second)),
                Some(second * 1000)
            );
        }
        // RDP user 2 types while console user 1 remains idle. Ordering does
        // not matter and the oldest/first session cannot authorize extraction.
        assert_eq!(
            clock.observe(
                Some(observation(&[(2, 40), (1, 500_000)])),
                now + Duration::from_secs(66),
            ),
            Some(40)
        );
        assert_eq!(
            classify_idle(
                Duration::from_millis(40),
                Duration::from_secs(15),
                Duration::from_secs(60),
            ),
            IdleState::Active
        );
    }

    #[test]
    fn clock_jumps_and_sampling_gaps_cannot_grant_instant_idleness() {
        let now = Instant::now();
        let mut clock = SessionClock::default();
        assert_eq!(clock.observe(Some(observation(&[(1, 0)])), now), Some(0));
        assert_eq!(
            clock.observe(Some(observation(&[(1, u64::MAX)])), now + Duration::from_secs(1)),
            Some(1000)
        );
        assert_eq!(
            clock.observe(Some(observation(&[(1, 10)])), now + Duration::from_secs(2)),
            Some(10)
        );
        // A long blocked worker, suspend, or observation failure cannot earn
        // idle credit for an interval in which activity was not observed.
        assert_eq!(
            clock.observe(Some(observation(&[(1, 500_000)])), now + Duration::from_secs(90)),
            Some(0)
        );
        assert_eq!(clock.observe(None, now + Duration::from_secs(91)), None);
        assert_eq!(
            clock.observe(Some(observation(&[(1, 500_000)])), now + Duration::from_secs(92)),
            Some(0)
        );
        // Reject backwards monotonic fixture input rather than underflowing.
        assert_eq!(clock.observe(Some(observation(&[(1, 500_000)])), now), Some(0));
    }

    #[test]
    fn unattended_machine_and_session_replacement_start_fresh_observation_windows() {
        let now = Instant::now();
        let mut clock = SessionClock::default();
        assert_eq!(clock.observe(Some(observation(&[])), now), Some(0));
        for second in 1..=65 {
            assert_eq!(
                clock.observe(Some(observation(&[])), now + Duration::from_secs(second)),
                Some(second * 1000)
            );
        }
        assert_eq!(
            clock.observe(Some(observation(&[(4, 500_000)])), now + Duration::from_secs(66)),
            Some(0)
        );
        assert_eq!(
            clock.observe(Some(observation(&[(4, 500_000)])), now + Duration::from_secs(67)),
            Some(1000)
        );
        for field in ["logon", "connected", "input"] {
            let mut replacement = observation(&[(4, 500_000)]);
            match field {
                "logon" => replacement.sessions[0].logon += 1,
                "connected" => replacement.sessions[0].connected += 1,
                "input" => replacement.sessions[0].input += 1,
                _ => unreachable!(),
            }
            assert_eq!(
                clock.observe(Some(replacement), now + Duration::from_secs(68)),
                Some(0)
            );
        }
    }

    #[test]
    fn incomplete_session_queries_are_not_confused_with_unattended_machine() {
        let now = Instant::now();
        let mut clock = SessionClock::default();
        for mut invalid in [observation(&[]), observation(&[(1, 1000)])] {
            invalid.idle = if invalid.sessions.is_empty() { Some(Duration::ZERO) } else { None };
            assert_eq!(clock.observe(Some(invalid), now), None);
            assert!(clock.previous.is_none());
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires Session 0 with an active console/RDP user and WTS query permissions"]
    fn native_session_zero_reads_interactive_user_timestamps() -> anyhow::Result<()> {
        use anyhow::ensure;
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::GetCurrentProcessId;

        let mut session = u32::MAX;
        // SAFETY: own process ID and a live output pointer.
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }?;
        ensure!(session == 0, "run this acceptance probe in service Session 0");
        let observation = native::observation()?;
        ensure!(
            !observation.sessions.is_empty(),
            "an active console/RDP user is required"
        );
        ensure!(observation.sessions.iter().all(|epoch| epoch.id != 0));
        ensure!(
            observation.idle.is_some(),
            "interactive input timestamps were not sampled"
        );
        Ok(())
    }
}
