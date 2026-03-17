use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, waitpid};
use nix::unistd::Pid;
use tokio::time::timeout;

use crate::error::{Result, SmuxError};
use crate::process::ExitStatus;

const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// Gracefully shut down a child process.
///
/// Cascade: SIGTERM → wait up to `grace` → SIGKILL → reap zombie.
pub async fn graceful_shutdown(pid: Pid, grace: Option<Duration>) -> Result<ExitStatus> {
    let grace = grace.unwrap_or(DEFAULT_GRACE);

    // Send SIGTERM
    let _ = kill(pid, Signal::SIGTERM);

    // Wait for the process to exit within the grace period
    let wait_result = timeout(grace, wait_for_exit(pid)).await;

    match wait_result {
        Ok(status) => Ok(status),
        Err(_elapsed) => {
            // Grace period expired — escalate to SIGKILL
            let _ = kill(pid, Signal::SIGKILL);
            // Now reap unconditionally
            Ok(reap_blocking(pid).await)
        }
    }
}

/// Async wrapper around blocking waitpid.
async fn wait_for_exit(pid: Pid) -> ExitStatus {
    tokio::task::spawn_blocking(move || crate::process::blocking_wait(pid))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or(ExitStatus::Unknown)
}

/// Reap the zombie with WNOHANG in a polling loop, then fall back to blocking.
async fn reap_blocking(pid: Pid) -> ExitStatus {
    // Try non-blocking first
    for _ in 0..5 {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => {
                return ExitStatus::Code(code);
            }
            Ok(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
                return ExitStatus::Signal(sig as i32);
            }
            Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            _ => break,
        }
    }
    // Fallback: blocking wait
    wait_for_exit(pid).await
}

/// Send a signal to a process.
pub fn send_signal(pid: Pid, signal: Signal) -> Result<()> {
    kill(pid, signal).map_err(SmuxError::Pty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn sigterm_exits_cleanly() {
        use crate::config::PtyConfig;
        use crate::pty::PtyProcess;

        let config = PtyConfig::new("/bin/sleep").args(["999"]);
        let pty = PtyProcess::spawn(&config).expect("spawn");
        let pid = pty.pid;
        // Prevent the drop impl from racing
        std::mem::forget(pty);

        let status = graceful_shutdown(pid, Some(Duration::from_millis(500))).await;
        assert!(status.is_ok());
    }
}
