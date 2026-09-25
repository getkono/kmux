//! Test-only helpers shared across this crate's unit tests (docs/testing.md R5).

use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;

/// How often [`wait_until_dead`] re-checks the process.
const POLL: Duration = Duration::from_millis(10);

/// Poll `kill(pid, 0)` until `pid` is gone or `deadline` passes. Returns
/// whether it is gone.
///
/// Replaces a fixed sleep before a liveness assertion: a dying process is
/// reported as soon as it is gone rather than after a guess at how long dying
/// takes, and a live one is reported once the deadline passes. The process is
/// always checked at least once, so a deadline already in the past is a
/// single, non-blocking probe.
///
/// "Gone" means out of the process table: a zombie still answers
/// `kill(pid, 0)`, so a child counts as dead only once something reaps it.
/// The helper never reaps, so it never competes for the `waitpid`: every child
/// `PtyProcess::spawn` starts is reaped by its exit task (`spawn_wait_task`),
/// whatever the code under test does. A `true` therefore proves the child was
/// killed; it does not prove the code under test is what reaped it.
///
/// Async so the probe yields to the runtime between checks: on a
/// current-thread `#[tokio::test]` the task that is supposed to kill or reap
/// the process could otherwise never run.
pub(crate) async fn wait_until_dead(pid: Pid, deadline: Instant) -> bool {
    loop {
        if kill(pid, None) == Err(Errno::ESRCH) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PtyConfig;
    use crate::pty::PtyProcess;
    use nix::sys::signal::Signal;

    #[tokio::test]
    async fn wait_until_dead_is_true_for_a_killed_child_and_false_for_a_live_one() {
        let live = PtyProcess::spawn(&PtyConfig::new("/bin/sleep").args(["30"])).expect("spawn");
        let doomed = PtyProcess::spawn(&PtyConfig::new("/bin/sleep").args(["30"])).expect("spawn");

        assert!(!wait_until_dead(live.pid, Instant::now()).await);

        kill(doomed.pid, Signal::SIGKILL).expect("kill");
        assert!(wait_until_dead(doomed.pid, Instant::now() + Duration::from_secs(5)).await);

        kill(live.pid, Signal::SIGKILL).expect("cleanup");
    }
}
