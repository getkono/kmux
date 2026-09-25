//! One reaper for every PTY child in the process.
//!
//! A single thread waits for `SIGCHLD` and, on each one, polls every child it
//! was asked to watch with `waitpid(pid, WNOHANG)`, publishing the exit status
//! to that child's `watch` channel. It replaces a blocking `waitpid` thread per
//! pane, which parked one of tokio's 512 blocking threads for each pane's whole
//! life.
//!
//! It polls the registered pids, never `waitpid(-1)`: other code in the same
//! process (`std::process::Child::wait` for a VT worker, `tokio::process`
//! for an SSH tunnel) reaps its own children, and a wildcard wait would steal
//! their exit statuses.
//!
//! The thread owns a small current-thread runtime, so it outlives whichever
//! runtime spawned the child — a test's runtime, or a daemon runtime being
//! torn down — and it is not a blocking-pool thread, so it never holds up a
//! runtime's shutdown.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use crate::error::{KmuxError, Result};
use crate::process::ExitStatus;

type ExitSender = watch::Sender<Option<ExitStatus>>;

/// The process-wide reaper.
pub(crate) struct Reaper {
    children: Mutex<HashMap<Pid, ExitSender>>,
}

static REAPER: OnceLock<std::result::Result<Reaper, String>> = OnceLock::new();

/// The reaper, started on first use.
///
/// Returns once its `SIGCHLD` handler is installed, so a child forked after
/// this returns cannot exit unobserved. Call it *before* forking.
pub(crate) fn reaper() -> Result<&'static Reaper> {
    REAPER
        .get_or_init(start)
        .as_ref()
        .map_err(|e| KmuxError::Spawn(format!("child reaper unavailable: {e}")))
}

fn start() -> std::result::Result<Reaper, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("kmux-pty-reaper".into())
        .spawn(move || run(&ready_tx))
        .map_err(|e| format!("spawn reaper thread: {e}"))?;
    ready_rx
        .recv()
        .map_err(|_| "reaper thread exited during startup".to_string())??;
    Ok(Reaper {
        children: Mutex::new(HashMap::new()),
    })
}

/// The reaper thread: install the `SIGCHLD` handler, report readiness, then
/// reap on every signal for the rest of the process's life.
fn run(ready: &std::sync::mpsc::Sender<std::result::Result<(), String>>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready.send(Err(format!("reaper runtime: {e}")));
            return;
        }
    };
    runtime.block_on(async {
        let mut sigchld = match signal(SignalKind::child()) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready.send(Err(format!("SIGCHLD handler: {e}")));
                return;
            }
        };
        let _ = ready.send(Ok(()));
        // `recv` returns `None` only if the signal driver goes away, which it
        // cannot while this runtime is blocked on this future.
        while sigchld.recv().await.is_some() {
            if let Some(Ok(reaper)) = REAPER.get() {
                reaper.reap_exited();
            }
        }
    });
}

impl Reaper {
    /// Watch `pid`, a child of this process, and return a receiver that
    /// becomes `Some(status)` once it has exited and been reaped.
    ///
    /// Checks the child once on the spot: had it exited before it was
    /// registered, its `SIGCHLD` has already come and gone.
    pub(crate) fn watch(&self, pid: Pid) -> watch::Receiver<Option<ExitStatus>> {
        let (tx, rx) = watch::channel(None);
        let mut children = self.lock();
        match classify(waitpid(pid, Some(WaitPidFlag::WNOHANG))) {
            Some(status) => {
                tx.send_replace(Some(status));
            }
            None => {
                children.insert(pid, tx);
            }
        }
        rx
    }

    /// Reap every watched child that has exited, and publish its status.
    fn reap_exited(&self) {
        self.lock().retain(
            |&pid, tx| match classify(waitpid(pid, Some(WaitPidFlag::WNOHANG))) {
                Some(status) => {
                    tx.send_replace(Some(status));
                    false
                }
                None => true,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Pid, ExitSender>> {
        self.children.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What one `waitpid(pid, WNOHANG)` says about a watched child: `None` while
/// it is still running (or merely stopped/continued), else how it ended.
///
/// `ECHILD` means the pid is no longer ours to wait on — something else reaped
/// it — so it is reported as ended with an unknown status rather than watched
/// forever. `EINTR` is retried on the next `SIGCHLD`.
fn classify(result: nix::Result<WaitStatus>) -> Option<ExitStatus> {
    match result {
        Ok(WaitStatus::Exited(_, code)) => Some(ExitStatus::Code(code)),
        Ok(WaitStatus::Signaled(_, signal, _)) => Some(ExitStatus::Signal(signal as i32)),
        Ok(_) | Err(Errno::EINTR) => None,
        Err(_) => Some(ExitStatus::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::signal::Signal;

    #[test]
    fn classify_maps_every_wait_status() {
        let pid = Pid::from_raw(1);
        let cases = [
            (Ok(WaitStatus::Exited(pid, 3)), Some(ExitStatus::Code(3))),
            (
                Ok(WaitStatus::Signaled(pid, Signal::SIGKILL, false)),
                Some(ExitStatus::Signal(9)),
            ),
            (Ok(WaitStatus::Stopped(pid, Signal::SIGTSTP)), None),
            (Ok(WaitStatus::StillAlive), None),
            (Err(Errno::EINTR), None),
            (Err(Errno::ECHILD), Some(ExitStatus::Unknown)),
        ];
        for (result, expected) in cases {
            assert_eq!(classify(result), expected, "{result:?}");
        }
    }

    /// Two children exiting in the opposite order to their spawning each get
    /// their own status on their own channel. Real children, because what is
    /// under test is the `SIGCHLD` → `waitpid` path (R7).
    #[tokio::test]
    async fn each_child_reports_its_own_status_whatever_the_exit_order() {
        use crate::config::PtyConfig;
        use crate::pty::PtyProcess;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        /// Let the child read a line and exit, then collect its status. The
        /// PTY is read to EOF first: a BSD tty's last close waits for queued
        /// output (here, the echoed line) to drain, so an unread master would
        /// keep the child from finishing its exit.
        async fn release(pty: &mut PtyProcess) -> ExitStatus {
            let deadline = std::time::Duration::from_secs(10);
            pty.io.write_all(b"go\n").await.expect("write");
            let mut buf = [0u8; 256];
            while let Ok(Ok(n)) = tokio::time::timeout(deadline, pty.io.read(&mut buf)).await {
                if n == 0 {
                    break;
                }
            }
            tokio::time::timeout(deadline, pty.wait())
                .await
                .expect("exit")
        }

        let spawn = |code: u8| {
            let script = format!("read line; exit {code}");
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", script.as_str()]))
                .expect("spawn")
        };
        let mut first = spawn(3);
        let mut second = spawn(5);

        assert_eq!(release(&mut second).await, ExitStatus::Code(5));
        assert!(
            !first.is_exited(),
            "the first child is still waiting for input"
        );
        assert_eq!(release(&mut first).await, ExitStatus::Code(3));
    }

    /// A child that exits before it is registered has already had its
    /// `SIGCHLD`; registering it must still report it. The child is left a
    /// zombie with `waitid(WNOWAIT)` first, so the exit has certainly happened.
    /// A real child, because a zombie is what is under test (R7).
    #[test]
    #[expect(
        clippy::zombie_processes,
        reason = "the child is left unwaited on purpose: the reaper under test reaps it"
    )]
    fn a_child_that_exited_before_registration_is_reported_at_once() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 7"])
            .spawn()
            .expect("spawn");
        let raw = i32::try_from(child.id()).expect("pid");
        // SAFETY: waits (without reaping) for a child this test spawned.
        let rc = unsafe {
            let mut info: nix::libc::siginfo_t = std::mem::zeroed();
            nix::libc::waitid(
                nix::libc::P_PID,
                nix::libc::id_t::try_from(raw).expect("id"),
                &raw mut info,
                nix::libc::WEXITED | nix::libc::WNOWAIT,
            )
        };
        assert_eq!(rc, 0, "waitid");

        let rx = reaper().expect("reaper").watch(Pid::from_raw(raw));
        assert_eq!(*rx.borrow(), Some(ExitStatus::Code(7)));
    }
}
