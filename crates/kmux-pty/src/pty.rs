use std::ffi::CString;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::pty::{ForkptyResult, forkpty};
use nix::unistd::{Pid, execve};
use tokio::sync::watch;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{KmuxError, Result};
use crate::io::PtyMasterIo;
use crate::platform::to_winsize;
use crate::process::{ExitStatus, spawn_wait_task};

/// A spawned PTY process.
///
/// Owns the master fd (wrapped in `PtyMasterIo`) and the child PID.
/// Dropping this struct triggers async cleanup (SIGKILL + fd close)
/// unless [`PtyProcess::set_keep_alive`] has been called to suppress it.
pub struct PtyProcess {
    /// Async I/O handle over the PTY master fd.
    pub io: PtyMasterIo,
    /// Child process PID.
    pub pid: Pid,
    /// Receiver that becomes `Some(ExitStatus)` when the child exits.
    pub exit_rx: watch::Receiver<Option<ExitStatus>>,
    /// Current window size.
    pub size: WindowSize,
    /// When `true`, the `Drop` impl skips SIGKILL so the child remains alive.
    ///
    /// Set this before dropping (e.g. on clean daemon shutdown) when the child
    /// PTY process should survive for reattachment on the next daemon start.
    keep_alive: AtomicBool,
}

impl PtyProcess {
    /// Spawn a child process in a new PTY.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let winsize = to_winsize(config.size);
        let env_map = config.env.clone().build();

        // Prepare C strings for exec
        let program = CString::new(config.program.as_str())
            .map_err(|_| KmuxError::Spawn("program name contains null byte".into()))?;

        let mut argv: Vec<CString> = Vec::with_capacity(config.args.len() + 1);
        argv.push(program.clone());
        for arg in &config.args {
            argv.push(
                CString::new(arg.as_str())
                    .map_err(|_| KmuxError::Spawn(format!("arg contains null byte: {arg}")))?,
            );
        }

        let envp: Vec<CString> = env_map
            .iter()
            .map(|(k, v)| {
                CString::new(format!("{k}={v}").as_str())
                    .map_err(|_| KmuxError::Spawn("env var contains null byte".into()))
            })
            .collect::<Result<Vec<_>>>()?;

        // Change working directory if specified
        if let Some(cwd) = &config.cwd {
            std::env::set_current_dir(cwd).map_err(KmuxError::Io)?;
        }

        // SAFETY: forkpty is unsafe; we uphold the contract by not using
        // tokio/threads in the child before exec, and by not sharing the
        // master fd across threads before returning from this function.
        let fork_result = unsafe { forkpty(Some(&winsize), None) }.map_err(KmuxError::Pty)?;

        match fork_result {
            ForkptyResult::Child => {
                // In child: exec the target program.
                // If exec fails, exit immediately to avoid double-cleanup.
                let _ = execve(&program, &argv, &envp);
                // exec failed -- exit child with error code
                unsafe { nix::libc::_exit(127) };
            }
            ForkptyResult::Parent { child, master } => {
                let master_fd: RawFd = master.into_raw_fd();
                let io = PtyMasterIo::new(master_fd).map_err(KmuxError::Io)?;
                let exit_rx = spawn_wait_task(child);
                Ok(PtyProcess {
                    io,
                    pid: child,
                    exit_rx,
                    size: config.size,
                    keep_alive: AtomicBool::new(false),
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

    /// When `true`, dropping this `PtyProcess` will NOT send SIGKILL to the
    /// child. The child process remains alive so a successor daemon can adopt
    /// its master fd via [`PtyProcess::from_inherited`] during a graceful handoff.
    pub fn set_keep_alive(&self, val: bool) {
        self.keep_alive.store(val, Ordering::Relaxed);
    }

    /// Whether keep-alive mode is enabled.
    pub fn is_keep_alive(&self) -> bool {
        self.keep_alive.load(Ordering::Relaxed)
    }

    /// Adopt a live PTY master fd inherited from another process (e.g. across a
    /// graceful daemon handoff via `SCM_RIGHTS`).
    ///
    /// The child is *foreign*: it is not a child of this process (it was
    /// reparented to init when the previous daemon exited), so `waitpid` cannot
    /// observe its exit. Liveness is therefore tracked by polling `kill(pid, 0)`
    /// (see [`spawn_kill_poll_task`]); the prompt, authoritative exit signal is
    /// the PTY master returning EOF in the relay loop. Portable across Linux and
    /// macOS (unlike the old `/proc/<pid>/fd` reopen, which this replaces).
    ///
    /// [`spawn_kill_poll_task`]: crate::process::spawn_kill_poll_task
    pub fn from_inherited(fd: std::os::fd::OwnedFd, pid: Pid, size: WindowSize) -> Result<Self> {
        let io = PtyMasterIo::new(fd.into_raw_fd()).map_err(KmuxError::Io)?;
        let exit_rx = crate::process::spawn_kill_poll_task(pid);
        Ok(PtyProcess {
            io,
            pid,
            exit_rx,
            size,
            keep_alive: AtomicBool::new(false),
        })
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.keep_alive.load(Ordering::Relaxed) {
            // Duplicate the PTY master fd before `PtyMasterIo` closes it.
            //
            // When the master fd closes the child receives SIGHUP (loss of
            // controlling terminal) which would kill it. Duplicating the fd
            // keeps the file description alive in the OS fd table so the child
            // can be reattached by the next daemon instance. The duplicate is
            // intentionally leaked (held open until this process exits), which
            // is safe since the daemon is shutting down immediately after.
            let raw = self.io.as_raw_fd();
            // SAFETY: `raw` is a valid, open fd owned by `self.io`.
            let dup_fd = unsafe { nix::libc::dup(raw) };
            // `dup_fd` is a plain i32 with no Drop impl, so it is intentionally
            // leaked — the fd stays open in the fd table.
            let _ = dup_fd;
            // Allow PtyMasterIo to drop (closes the original fd) without
            // sending SIGKILL. The dup above keeps the terminal alive.
            return;
        }

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

    fn sleep_config() -> PtyConfig {
        PtyConfig::new("/bin/sleep").args(["30"])
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

    /// A PTY master fd duplicated via `dup_owned` and adopted with
    /// `from_inherited` drives the *same* live child: writing to the inherited
    /// master and reading its echo proves the dup is a fully-functional handle,
    /// and the child stays alive after the original is dropped with keep-alive —
    /// the core invariant behind live PTY migration across a daemon handoff.
    #[tokio::test]
    async fn from_inherited_drives_the_same_live_child() {
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // `cat` echoes its stdin straight back to stdout.
        let original = PtyProcess::spawn(&PtyConfig::new("/bin/cat")).expect("spawn");
        let pid = original.pid;
        let size = original.size;

        let dup = original.io.dup_owned().expect("dup master");
        let mut inherited = PtyProcess::from_inherited(dup, pid, size).expect("from_inherited");

        // Relinquish the original without killing the child (as a handoff would).
        original.set_keep_alive(true);
        drop(original);

        inherited.io.write_all(b"hello\n").await.expect("write");

        let mut seen = String::new();
        let mut buf = [0u8; 256];
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), inherited.io.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    seen.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if seen.contains("hello") {
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            seen.contains("hello"),
            "inherited PTY should echo input; got {seen:?}"
        );
        assert!(
            nix::sys::signal::kill(pid, None).is_ok(),
            "child should still be alive after the original was dropped"
        );

        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    /// A foreign (inherited) child cannot be `waitpid`-ed, so exit is detected by
    /// polling `kill(pid, 0)`. `wait()` must still resolve once the child exits.
    #[tokio::test]
    async fn from_inherited_detects_foreign_child_exit() {
        use std::time::Duration;

        let original =
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", "sleep 0.2"])).expect("spawn");
        let pid = original.pid;
        let size = original.size;

        let dup = original.io.dup_owned().expect("dup master");
        let mut inherited = PtyProcess::from_inherited(dup, pid, size).expect("from_inherited");
        original.set_keep_alive(true);
        drop(original);

        let waited = tokio::time::timeout(Duration::from_secs(3), inherited.wait()).await;
        assert!(
            waited.is_ok(),
            "foreign child exit was not detected in time"
        );
        assert!(inherited.is_exited(), "is_exited should be true after exit");
    }

    /// Verify that setting `keep_alive` prevents the Drop impl from sending
    /// SIGKILL: the child process should still be running after the
    /// `PtyProcess` is dropped with `keep_alive = true`.
    #[tokio::test]
    async fn keep_alive_prevents_sigkill_on_drop() {
        let pty = PtyProcess::spawn(&sleep_config()).expect("spawn failed");
        let pid = pty.pid;

        pty.set_keep_alive(true);
        drop(pty);

        // Give the tokio runtime a moment to process any drop task.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // The process should still be alive: kill(pid, 0) succeeds.
        let alive = nix::sys::signal::kill(pid, None).is_ok();
        assert!(alive, "process should still be alive after keep_alive drop");

        // Clean up: kill the process ourselves.
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }
}
