use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::time;

use crate::error::kmuxError;
use crate::process::ExitStatus;

/// Timeout enforcement for PTY sessions.
///
/// Monitors wall-clock and idle timeouts, sending SIGKILL if exceeded.
pub struct TimeoutEnforcer {
    wall_clock: Option<Duration>,
    idle: Option<Duration>,
    last_activity: Instant,
}

impl TimeoutEnforcer {
    pub fn new(wall_clock: Option<Duration>, idle: Option<Duration>) -> Self {
        Self {
            wall_clock,
            idle,
            last_activity: Instant::now(),
        }
    }

    /// Record that I/O activity occurred (resets idle timer).
    pub fn record_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check if any timeout has elapsed. Returns the appropriate error if so.
    pub fn check(&self, started_at: Instant) -> Option<kmuxError> {
        if let Some(wall) = self.wall_clock
            && started_at.elapsed() >= wall
        {
            return Some(kmuxError::Timeout);
        }
        if let Some(idle) = self.idle {
            let elapsed = self.last_activity.elapsed();
            if elapsed >= idle {
                return Some(kmuxError::IdleTimeout {
                    seconds: elapsed.as_secs(),
                });
            }
        }
        None
    }

    /// Run a background timeout watcher that kills the process if a timeout fires.
    ///
    /// Returns a watch receiver that completes when the watcher ends.
    pub fn spawn_watcher(
        wall_clock: Option<Duration>,
        idle_duration: Option<Duration>,
        pid: nix::unistd::Pid,
        exit_rx: watch::Receiver<Option<ExitStatus>>,
    ) -> watch::Receiver<Option<kmuxError>> {
        let (tx, rx) = watch::channel(None);

        tokio::spawn(async move {
            let started = Instant::now();
            let tick = Duration::from_millis(100);
            let last_exit_check = exit_rx;

            loop {
                // Stop watching if the process already exited
                if last_exit_check.borrow().is_some() {
                    break;
                }

                time::sleep(tick).await;

                // Wall-clock check
                if let Some(wall) = wall_clock
                    && started.elapsed() >= wall
                {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = tx.send(Some(kmuxError::Timeout));
                    break;
                }

                // Idle timeout requires external activity notification -- we check
                // a simpler model here: if wall_clock is the only check, idle
                // handling is done by the session layer.
                let _ = idle_duration; // used at session layer
            }
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_fires() {
        let enforcer = TimeoutEnforcer::new(Some(Duration::from_millis(1)), None);
        std::thread::sleep(Duration::from_millis(5));
        let started = Instant::now() - Duration::from_millis(10);
        assert!(enforcer.check(started).is_some());
    }

    #[test]
    fn idle_fires() {
        let enforcer = TimeoutEnforcer::new(None, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        let started = Instant::now();
        assert!(enforcer.check(started).is_some());
    }

    #[test]
    fn no_timeout_when_within_limits() {
        let enforcer =
            TimeoutEnforcer::new(Some(Duration::from_secs(60)), Some(Duration::from_secs(60)));
        let started = Instant::now();
        assert!(enforcer.check(started).is_none());
    }
}
