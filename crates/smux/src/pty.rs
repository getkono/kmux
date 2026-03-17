use std::ffi::CString;
use std::os::unix::io::{IntoRawFd, RawFd};

use nix::pty::{ForkptyResult, forkpty};
use nix::unistd::{Pid, execvpe};
use tokio::sync::watch;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{Result, SmuxError};
use crate::io::PtyMasterIo;
use crate::platform::to_winsize;
use crate::process::{ExitStatus, spawn_wait_task};

/// A spawned PTY process.
///
/// Owns the master fd (wrapped in `PtyMasterIo`) and the child PID.
/// Dropping this struct triggers async cleanup (SIGKILL + fd close).
pub struct PtyProcess {
    /// Async I/O handle over the PTY master fd.
    pub io: PtyMasterIo,
    /// Child process PID.
    pub pid: Pid,
    /// Receiver that becomes `Some(ExitStatus)` when the child exits.
    pub exit_rx: watch::Receiver<Option<ExitStatus>>,
    /// Current window size.
    pub size: WindowSize,
}

impl PtyProcess {
    /// Spawn a child process in a new PTY.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let winsize = to_winsize(config.size);
        let env_map = config.env.clone().build();

        // Prepare C strings for exec
        let program = CString::new(config.program.as_str())
            .map_err(|_| SmuxError::Spawn("program name contains null byte".into()))?;

        let mut argv: Vec<CString> = Vec::with_capacity(config.args.len() + 1);
        argv.push(program.clone());
        for arg in &config.args {
            argv.push(
                CString::new(arg.as_str())
                    .map_err(|_| SmuxError::Spawn(format!("arg contains null byte: {arg}")))?,
            );
        }

        let envp: Vec<CString> = env_map
            .iter()
            .map(|(k, v)| {
                CString::new(format!("{k}={v}").as_str())
                    .map_err(|_| SmuxError::Spawn("env var contains null byte".into()))
            })
            .collect::<Result<Vec<_>>>()?;

        // Change working directory if specified
        if let Some(cwd) = &config.cwd {
            std::env::set_current_dir(cwd).map_err(SmuxError::Io)?;
        }

        // SAFETY: forkpty is unsafe; we uphold the contract by not using
        // tokio/threads in the child before exec, and by not sharing the
        // master fd across threads before returning from this function.
        let fork_result = unsafe { forkpty(Some(&winsize), None) }.map_err(SmuxError::Pty)?;

        match fork_result {
            ForkptyResult::Child => {
                // In child: exec the target program.
                // If exec fails, exit immediately to avoid double-cleanup.
                let _ = execvpe(&program, &argv, &envp);
                // exec failed — exit child with error code
                unsafe { nix::libc::_exit(127) };
            }
            ForkptyResult::Parent { child, master } => {
                let master_fd: RawFd = master.into_raw_fd();
                let io = PtyMasterIo::new(master_fd).map_err(SmuxError::Io)?;
                let exit_rx = spawn_wait_task(child);
                Ok(PtyProcess {
                    io,
                    pid: child,
                    exit_rx,
                    size: config.size,
                })
            }
        }
    }

    /// Resize the PTY window.
    pub fn resize(&mut self, size: WindowSize) -> Result<()> {
        crate::resize::resize_pty(self.io.as_raw_fd(), size)?;
        self.size = size;
        Ok(())
    }

    /// Check if the child process has exited.
    pub fn is_exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }

    /// Wait asynchronously for the child process to exit.
    pub async fn wait(&mut self) -> ExitStatus {
        loop {
            if let Some(status) = self.exit_rx.borrow().clone() {
                return status;
            }
            // Wait for the channel to update
            if self.exit_rx.changed().await.is_err() {
                return ExitStatus::Unknown;
            }
        }
    }

    /// Return the raw PTY master fd (for advanced use).
    pub fn master_fd(&self) -> RawFd {
        self.io.as_raw_fd()
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if !self.is_exited() {
            // Spawn a detached cleanup task: send SIGKILL, reap zombie
            let pid = self.pid;
            tokio::spawn(async move {
                let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                // Give the kernel a moment, then reap
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_config() -> PtyConfig {
        PtyConfig::new("/bin/echo").args(["hello"])
    }

    #[tokio::test]
    async fn spawn_echo_exits_zero() {
        let mut pty = PtyProcess::spawn(&echo_config()).expect("spawn failed");
        let status = pty.wait().await;
        assert!(status.success(), "expected exit code 0, got {status}");
    }

    #[tokio::test]
    async fn spawn_reads_output() {
        use tokio::io::AsyncReadExt;

        let mut pty = PtyProcess::spawn(&echo_config()).expect("spawn failed");
        let mut output = Vec::new();
        let mut buf = [0u8; 256];

        loop {
            match pty.io.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello"),
            "expected 'hello' in output, got: {text:?}"
        );
    }
}
