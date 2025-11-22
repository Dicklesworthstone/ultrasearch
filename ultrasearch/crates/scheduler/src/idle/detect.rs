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
    /// How long the user has been idle according to the platform timer.
    pub idle_for: Duration,
    /// Time elapsed since we last transitioned into this state.
    pub since_state_change: Duration,
}

/// Tracks user activity using platform idle time (GetLastInputInfo on Windows).
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

    /// Read the current idle state and update transition bookkeeping.
    pub fn sample(&mut self) -> IdleSample {
        let idle_for = (self.reader)()
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(0));

        let state = classify_idle(idle_for, self.warm_idle, self.deep_idle);

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
    use tracing::warn;
    use windows::Win32::System::SystemInformation::GetTickCount64;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    // SAFETY: GetLastInputInfo expects a properly initialized struct.
    unsafe {
        let mut info = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };

        if GetLastInputInfo(&mut info).as_bool() {
            let now = GetTickCount64();
            let last = info.dwTime as u64;
            return now.checked_sub(last);
        }
    }

    warn!("GetLastInputInfo failed; treating as active");
    None
}

#[cfg(not(target_os = "windows"))]
fn idle_elapsed_ms() -> Option<u64> {
    // Non-Windows platforms: caller can still inject a reader in tests;
    // default fallback is "active".
    None
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
}
