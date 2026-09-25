use std::time::Duration;

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::error::{KmuxError, Result};
use crate::process::ExitStatus;

const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// Shut down a PTY child and everything in its process group.
///
/// The child is a session leader, so its pid is also its process group id,
/// and signalling the group reaches the jobs and pipelines it started, not
/// only the shell. The cascade:
///
/// 1. `SIGHUP` then `SIGTERM` to the group. `SIGHUP` is what closing a
///    terminal means: an interactive shell ignores `SIGTERM` but exits on
///    `SIGHUP`, passing it on to its own jobs.
/// 2. Wait up to `grace` for the child to exit.
/// 3. `SIGKILL` the group, which also ends any member that outlived the
///    child by ignoring both signals.
/// 4. If the child had not yet exited, wait up to `grace` again for the
///    `SIGKILL` to land, and report [`ExitStatus::Unknown`] if even that does
///    not (a process stuck in uninterruptible sleep).
///
/// `exit_rx` is the child's exit channel: fed by the reaper for a child of
/// this process, and by a `kill(pid, 0)` poll for one inherited across a
/// handoff. Either way exit means "gone", never "not ours to wait on" — an
/// inherited child cannot be `waitpid`-ed, and `ECHILD` from trying to is not
/// evidence that it died.
pub async fn graceful_shutdown(
    pid: Pid,
    mut exit_rx: watch::Receiver<Option<ExitStatus>>,
    grace: Option<Duration>,
) -> ExitStatus {
    let grace = grace.unwrap_or(DEFAULT_GRACE);
    signal_group(pid, Signal::SIGHUP);
    signal_group(pid, Signal::SIGTERM);
    if let Ok(status) = timeout(grace, wait_for_exit(&mut exit_rx)).await {
        // The child is reaped and its pid free for reuse, so only the group is
        // signalled: its id stays reserved while it has members.
        let _ = killpg(pid, Signal::SIGKILL);
        return status;
    }
    signal_group(pid, Signal::SIGKILL);
    timeout(grace, wait_for_exit(&mut exit_rx))
        .await
        .unwrap_or(ExitStatus::Unknown)
}

/// Send `signal` to the PTY child's process group, and to the child itself.
///
/// The child becomes a session leader, and so gets its own process group,
/// only once it has run `setsid` — which may not have happened yet when a
/// pane is closed the moment it opens. Until then there is no group to
/// signal (`ESRCH`), but the pid is already the child's, and the child has
/// not yet started anything else that would need reaching.
pub(crate) fn signal_group(pid: Pid, signal: Signal) {
    let _ = killpg(pid, signal);
    let _ = kill(pid, signal);
}

/// Resolve once `exit_rx` reports an exit. A channel whose sender is gone
/// without reporting one resolves to [`ExitStatus::Unknown`].
async fn wait_for_exit(exit_rx: &mut watch::Receiver<Option<ExitStatus>>) -> ExitStatus {
    exit_rx
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|status| status.clone())
        .unwrap_or(ExitStatus::Unknown)
}

/// Send a signal to a process.
pub fn send_signal(pid: Pid, signal: Signal) -> Result<()> {
    kill(pid, signal).map_err(KmuxError::Pty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    use tokio::io::AsyncReadExt;

    use crate::config::PtyConfig;
    use crate::pty::PtyProcess;

    /// Generous: every wait below ends as soon as its condition holds.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// Read the PTY until `marker` has appeared, returning everything read.
    async fn read_until(pty: &mut PtyProcess, marker: &str) -> String {
        let mut seen = String::new();
        let mut buf = [0u8; 256];
        while !seen.contains(marker) {
            let n = timeout(DEADLINE, pty.io.read(&mut buf))
                .await
                .expect("output in time")
                .expect("read");
            assert_ne!(n, 0, "EOF before {marker:?}; got {seen:?}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        seen
    }

    /// The pid a shell printed on the line after `marker`.
    fn pid_after(output: &str, marker: &str) -> Pid {
        let (_, rest) = output.split_once(marker).expect("marker");
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Pid::from_raw(digits.parse().expect("a pid"))
    }

    /// A shell that exits on the first signal ends the cascade there and
    /// reports how it died. Needs a real child: the status comes from the
    /// kernel (R7).
    #[tokio::test]
    async fn graceful_shutdown_reports_the_signal_that_ended_the_child() {
        let pty = PtyProcess::spawn(&PtyConfig::new("/bin/sleep").args(["600"])).expect("spawn");
        let status = graceful_shutdown(pty.pid, pty.exit_rx.clone(), Some(DEADLINE)).await;
        assert_eq!(status, ExitStatus::Signal(Signal::SIGHUP as i32));
    }

    /// A background job is in the shell's process group, so closing reaches
    /// it too. This one ignores `SIGHUP` — as a job does after `nohup` — so
    /// the kernel's hangup of the foreground group on the shell's exit would
    /// not end it: only the `SIGTERM` sent to the whole group does. Needs a
    /// real shell and process group (R7).
    #[tokio::test]
    async fn graceful_shutdown_ends_the_whole_process_group() {
        let script = "trap '' HUP; sleep 600 & echo \"bg=$!\"; trap - HUP; wait";
        let mut pty =
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", script])).expect("spawn");
        let background = pid_after(&read_until(&mut pty, "bg=").await, "bg=");

        graceful_shutdown(pty.pid, pty.exit_rx.clone(), Some(DEADLINE)).await;

        let gone = crate::fixtures::wait_until_dead(background, Instant::now() + DEADLINE).await;
        assert!(gone, "background job {background} outlived the pane");
    }

    /// A job that ignores `SIGHUP` and `SIGTERM` in a group whose leader
    /// exited on the first signal is still `SIGKILL`ed once the child is
    /// gone. Needs a real process group (R7).
    #[tokio::test]
    async fn graceful_shutdown_kills_a_member_that_outlives_the_child() {
        let script = "trap '' HUP TERM; sleep 600 & echo \"bg=$!\"; trap - HUP TERM; wait";
        let mut pty =
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", script])).expect("spawn");
        let background = pid_after(&read_until(&mut pty, "bg=").await, "bg=");

        graceful_shutdown(pty.pid, pty.exit_rx.clone(), Some(DEADLINE)).await;

        let gone = crate::fixtures::wait_until_dead(background, Instant::now() + DEADLINE).await;
        assert!(gone, "a job ignoring HUP and TERM outlived the pane");
    }

    /// After a handoff the child is not ours to `waitpid`. A shell that
    /// ignores `SIGHUP` and `SIGTERM` must still be `SIGKILL`ed after the
    /// grace period and confirmed dead by polling, not taken for exited
    /// because `waitpid` says `ECHILD`. Needs a real child adopted through
    /// `from_inherited` (R7).
    #[tokio::test]
    async fn graceful_shutdown_kills_an_inherited_child_that_ignores_sigterm() {
        let script = "trap '' HUP TERM; echo ready; sleep 600";
        let mut original =
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", script])).expect("spawn");
        read_until(&mut original, "ready").await;
        let (pid, size) = (original.pid, original.size);
        let inherited =
            PtyProcess::from_inherited(original.io.dup_owned().expect("dup"), pid, size)
                .expect("from_inherited");
        original.set_keep_alive(true);
        drop(original);

        let started = Instant::now();
        let grace = Duration::from_millis(100);
        let status = graceful_shutdown(pid, inherited.exit_rx.clone(), Some(grace)).await;

        assert!(
            started.elapsed() >= grace,
            "returned before the grace period"
        );
        assert_eq!(
            status,
            ExitStatus::Unknown,
            "an inherited child's reason is unknowable"
        );
        assert!(crate::fixtures::wait_until_dead(pid, Instant::now() + DEADLINE).await);
    }
}
