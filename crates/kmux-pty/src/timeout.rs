use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::time;

use crate::error::KmuxError;
use crate::process::ExitStatus;

/// Timeout enforcement for PTY sessions.
///
/// Monitors wall-clock and idle timeouts, sending SIGKILL if exceeded.
///
/// Pure over time: every method takes the current `Instant` as a parameter
/// rather than reading the clock, so the policy is testable from a fixed base
/// plus explicit offsets (docs/testing.md R3). [`TimeoutEnforcer::spawn_watcher`]
/// is the one place that reads the real clock.
pub struct TimeoutEnforcer {
    wall_clock: Option<Duration>,
    idle: Option<Duration>,
    started_at: Instant,
    last_activity: Instant,
}

impl TimeoutEnforcer {
    /// A policy for a session that started at `started_at`, which also counts
    /// as its first activity.
    pub fn new(wall_clock: Option<Duration>, idle: Option<Duration>, started_at: Instant) -> Self {
        Self {
            wall_clock,
            idle,
            started_at,
            last_activity: started_at,
        }
    }

    /// Record that I/O activity occurred at `now` (resets the idle timer).
    pub fn record_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// Check whether any timeout has elapsed as of `now`. Returns the
    /// appropriate error if so; the wall-clock limit is checked first.
    pub fn check(&self, now: Instant) -> Option<KmuxError> {
        if let Some(wall) = self.wall_clock
            && now.saturating_duration_since(self.started_at) >= wall
        {
            return Some(KmuxError::Timeout);
        }
        if let Some(idle) = self.idle {
            let elapsed = now.saturating_duration_since(self.last_activity);
            if elapsed >= idle {
                return Some(KmuxError::IdleTimeout {
                    seconds: elapsed.as_secs(),
                });
            }
        }
        None
    }

    /// Run a background timeout watcher that kills the process if the
    /// wall-clock timeout fires.
    ///
    /// Idle timeouts need activity notifications the watcher does not see, so
    /// they are enforced by the session layer; `idle_duration` is accepted for
    /// signature symmetry and not acted on here.
    ///
    /// Returns a watch receiver that completes when the watcher ends.
    pub fn spawn_watcher(
        wall_clock: Option<Duration>,
        idle_duration: Option<Duration>,
        pid: nix::unistd::Pid,
        exit_rx: watch::Receiver<Option<ExitStatus>>,
    ) -> watch::Receiver<Option<KmuxError>> {
        let (tx, rx) = watch::channel(None);
        let _ = idle_duration; // enforced by the session layer, see above
        let enforcer = Self::new(wall_clock, None, Instant::now());

        tokio::spawn(async move {
            let tick = Duration::from_millis(100);

            loop {
                // Stop watching if the process already exited
                if exit_rx.borrow().is_some() {
                    break;
                }

                time::sleep(tick).await;

                if let Some(err) = enforcer.check(Instant::now()) {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = tx.send(Some(err));
                    break;
                }
            }
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn check_at_the_wall_clock_limit_is_timeout() {
        let base = Instant::now();
        let enforcer = TimeoutEnforcer::new(Some(10 * SECOND), None, base);
        assert!(
            enforcer
                .check(base + 10 * SECOND - Duration::from_millis(1))
                .is_none()
        );
        assert!(matches!(
            enforcer.check(base + 10 * SECOND),
            Some(KmuxError::Timeout)
        ));
    }

    #[test]
    fn check_after_idle_limit_is_idle_timeout_with_elapsed_seconds() {
        let base = Instant::now();
        let enforcer = TimeoutEnforcer::new(None, Some(5 * SECOND), base);
        assert!(enforcer.check(base + 4 * SECOND).is_none());
        assert!(matches!(
            enforcer.check(base + 7 * SECOND),
            Some(KmuxError::IdleTimeout { seconds: 7 })
        ));
    }

    #[test]
    fn record_activity_restarts_the_idle_window() {
        let base = Instant::now();
        let mut enforcer = TimeoutEnforcer::new(None, Some(5 * SECOND), base);
        enforcer.record_activity(base + 4 * SECOND);
        assert!(enforcer.check(base + 8 * SECOND).is_none());
        assert!(matches!(
            enforcer.check(base + 9 * SECOND),
            Some(KmuxError::IdleTimeout { seconds: 5 })
        ));
    }

    #[test]
    fn check_within_limits_or_without_limits_is_none() {
        let base = Instant::now();
        let bounded = TimeoutEnforcer::new(Some(60 * SECOND), Some(60 * SECOND), base);
        assert!(bounded.check(base + 59 * SECOND).is_none());
        let unbounded = TimeoutEnforcer::new(None, None, base);
        assert!(unbounded.check(base + 3600 * SECOND).is_none());
    }
}
