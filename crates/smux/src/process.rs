use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use tokio::sync::watch;

use crate::error::{Result, SmuxError};

/// Rich exit status for a PTY child process.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ExitStatus {
    /// Process exited normally with the given exit code.
    Code(i32),
    /// Process was terminated by a signal.
    Signal(i32),
    /// Process state is unknown (e.g., stopped).
    Unknown,
}

impl ExitStatus {
    /// Returns `true` if the process exited with code 0.
    pub fn success(&self) -> bool {
        matches!(self, ExitStatus::Code(0))
    }

    /// Returns the exit code if this is a normal exit.
    pub fn code(&self) -> Option<i32> {
        match self {
            ExitStatus::Code(c) => Some(*c),
            _ => None,
        }
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitStatus::Code(c) => write!(f, "exit code {c}"),
            ExitStatus::Signal(s) => write!(f, "killed by signal {s}"),
            ExitStatus::Unknown => write!(f, "unknown exit status"),
        }
    }
}

/// Wait for a child process to exit, blocking the current thread.
///
/// This is called from a `spawn_blocking` task so it doesn't stall the async executor.
pub fn blocking_wait(pid: Pid) -> Result<ExitStatus> {
    loop {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, code)) => return Ok(ExitStatus::Code(code)),
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                return Ok(ExitStatus::Signal(sig as i32));
            }
            Ok(WaitStatus::Stopped(_, _)) => {
                // Child stopped (SIGSTOP/SIGTSTP) -- keep waiting
                continue;
            }
            Ok(_) => return Ok(ExitStatus::Unknown),
            Err(nix::Error::EINTR) => continue, // Interrupted, retry
            Err(e) => return Err(SmuxError::Pty(e)),
        }
    }
}

/// Spawn an async task that waits for a child PID and signals completion
/// via a `watch` channel.
///
/// Returns a receiver that yields `Some(ExitStatus)` when the child exits.
pub fn spawn_wait_task(pid: Pid) -> watch::Receiver<Option<ExitStatus>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || blocking_wait(pid)).await;
        let status = match result {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => ExitStatus::Unknown,
            Err(_) => ExitStatus::Unknown,
        };
        // Ignore send errors -- receiver may have been dropped
        let _ = tx.send(Some(status));
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_success() {
        assert!(ExitStatus::Code(0).success());
        assert!(!ExitStatus::Code(1).success());
        assert!(!ExitStatus::Signal(9).success());
    }

    #[test]
    fn exit_status_code() {
        assert_eq!(ExitStatus::Code(42).code(), Some(42));
        assert_eq!(ExitStatus::Signal(9).code(), None);
    }

    #[test]
    fn exit_status_display() {
        assert_eq!(ExitStatus::Code(0).to_string(), "exit code 0");
        assert_eq!(ExitStatus::Signal(9).to_string(), "killed by signal 9");
    }
}
