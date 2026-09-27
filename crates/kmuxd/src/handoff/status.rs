//! Where this daemon's graceful handoffs stand, for the control socket
//! (issue #207): whether one runs (a second `restart` is `busy`), and why the
//! latest stood down, so `kmux daemon restart` can report it rather than time
//! out on it.

use std::sync::{Mutex, PoisonError};

use kmux_protocol::control_rpc::HandoffReport;

/// The daemon's handoffs: at most one at a time, numbered from 1.
#[derive(Debug, Default)]
pub struct HandoffStatus {
    report: Mutex<HandoffReport>,
}

impl HandoffStatus {
    /// Begin a handoff, and return its number — or `None` while one runs.
    pub fn begin(&self) -> Option<u64> {
        let mut report = self.lock();
        if report.in_progress {
            return None;
        }
        report.attempt += 1;
        report.in_progress = true;
        report.stood_down = None;
        Some(report.attempt)
    }

    /// The handoff rolled back, for `why`: this daemon serves on, and a new
    /// handoff may begin.
    pub fn rolled_back(&self, why: String) {
        let mut report = self.lock();
        report.in_progress = false;
        report.stood_down = Some(why);
    }

    /// Where the latest handoff stands.
    pub fn report(&self) -> HandoffReport {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HandoffReport> {
        self.report.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A successor that stood down: the predecessor serves on (or is shutting
/// down), so this daemon exits without serving, with
/// [`kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE`].
#[derive(Debug)]
pub struct StoodDown(pub String);

impl std::fmt::Display for StoodDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handoff stood down: {}", self.0)
    }
}

impl std::error::Error for StoodDown {}

#[cfg(test)]
mod tests {
    use super::*;

    /// One handoff at a time: a second is refused while the first runs, and
    /// a rollback reports why and lets the next one begin, numbered on.
    #[test]
    fn handoffs_run_one_at_a_time_and_report_a_rollback() {
        let status = HandoffStatus::default();
        assert_eq!(status.report(), HandoffReport::default());

        assert_eq!(status.begin(), Some(1));
        assert_eq!(status.begin(), None, "busy");
        let running = status.report();
        assert!(running.in_progress);
        assert_eq!(running.attempt, 1);

        status.rolled_back("no Ack".into());
        let report = status.report();
        assert!(!report.in_progress);
        assert_eq!(report.stood_down.as_deref(), Some("no Ack"));

        assert_eq!(status.begin(), Some(2));
        assert_eq!(status.report().stood_down, None, "cleared for the new one");
    }

    #[test]
    fn a_stand_down_says_so() {
        let stood = StoodDown("the predecessor rolled back".into());
        assert_eq!(
            stood.to_string(),
            "handoff stood down: the predecessor rolled back"
        );
    }
}
