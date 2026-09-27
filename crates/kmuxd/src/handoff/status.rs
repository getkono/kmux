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
    /// Begin a handoff, or `None` while one runs. The handoff counts as
    /// running from here, so a second `restart` is `busy`; until
    /// [`BegunHandoff::hand_over`] passes it to the daemon's main task, the
    /// returned guard rolls it back when dropped, so a restart request cut
    /// off before the handoff started never leaves every later one `busy`.
    pub fn begin(&self) -> Option<BegunHandoff<'_>> {
        let mut report = self.lock();
        if report.in_progress {
            return None;
        }
        report.attempt += 1;
        report.in_progress = true;
        report.stood_down = None;
        Some(BegunHandoff {
            status: self,
            attempt: report.attempt,
            handed_over: false,
        })
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

/// Why a handoff rolled back whose request was cut off before it started.
pub const CUT_OFF: &str = "the restart request was cut off before the handoff started";

/// A handoff [`HandoffStatus::begin`] started and nobody has run yet.
#[derive(Debug)]
#[must_use = "dropping it rolls the handoff back"]
pub struct BegunHandoff<'a> {
    status: &'a HandoffStatus,
    attempt: u64,
    handed_over: bool,
}

impl BegunHandoff<'_> {
    /// The handoff's number.
    pub fn attempt(&self) -> u64 {
        self.attempt
    }

    /// The daemon's main task has been told to run it, and owns it now: it
    /// reports the outcome.
    pub fn hand_over(mut self) {
        self.handed_over = true;
    }
}

impl Drop for BegunHandoff<'_> {
    fn drop(&mut self) {
        if !self.handed_over {
            self.status.rolled_back(CUT_OFF.to_string());
        }
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

        let first = status.begin().expect("the first begins");
        assert_eq!(first.attempt(), 1);
        first.hand_over();
        assert!(status.begin().is_none(), "busy");
        let running = status.report();
        assert!(running.in_progress);
        assert_eq!(running.attempt, 1);

        status.rolled_back("no Ack".into());
        let report = status.report();
        assert!(!report.in_progress);
        assert_eq!(report.stood_down.as_deref(), Some("no Ack"));

        let second = status.begin().expect("a new one begins");
        assert_eq!(second.attempt(), 2);
        second.hand_over();
        assert_eq!(status.report().stood_down, None, "cleared for the new one");
    }

    /// A handoff begun and never handed over (its restart request was cut
    /// off) rolls back, so the next restart is not refused as busy.
    #[test]
    fn a_handoff_never_handed_over_rolls_back() {
        let status = HandoffStatus::default();
        let cut_off = status.begin().expect("begins");
        assert!(status.report().in_progress);
        drop(cut_off);

        let report = status.report();
        assert!(!report.in_progress);
        assert_eq!(report.stood_down.as_deref(), Some(CUT_OFF));
        let next = status.begin().expect("not busy");
        assert_eq!(next.attempt(), 2);
        next.hand_over();
        assert!(
            status.report().in_progress,
            "a handed-over one keeps running"
        );
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
