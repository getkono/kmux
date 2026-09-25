use std::os::unix::io::{IntoRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use nix::pty::{ForkptyResult, forkpty};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::sync::watch;

use crate::child::ChildPlan;
use crate::config::{PtyConfig, WindowSize};
use crate::error::{KmuxError, Result};
use crate::io::PtyMasterIo;
use crate::platform::to_winsize;
use crate::process::ExitStatus;
use crate::reaper::reaper;
use crate::shutdown::signal_group;

/// A spawned PTY process.
///
/// Owns the master fd (wrapped in `PtyMasterIo`) and the child PID.
/// Dropping this struct SIGKILLs the child's process group and closes the fd,
/// unless [`PtyProcess::set_keep_alive`] has been called to suppress the kill.
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
    /// Set this before dropping when another process now owns the child: a
    /// handoff successor, or (inside a VT worker) the daemon.
    keep_alive: AtomicBool,
}

impl PtyProcess {
    /// Spawn a child process in a new PTY.
    ///
    /// The child is a session leader (and so its own process group) with the
    /// PTY as its controlling terminal. Before it runs the program it resets
    /// the daemon's signal dispositions and mask, closes every inherited fd
    /// past stdio, and changes to `config.cwd` — all in the child, so the
    /// daemon's own working directory never changes. If it cannot change
    /// directory or run the program it writes why to the PTY and exits 127.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let winsize = to_winsize(config.size);
        let plan = ChildPlan::new(config)?;
        // Before the fork, so the SIGCHLD handler is in place however soon the
        // child exits.
        let reaper = reaper()?;

        // SAFETY: the child branch calls only `ChildPlan::exec`, which makes
        // async-signal-safe calls on memory prepared before the fork.
        let fork_result = unsafe { forkpty(Some(&winsize), None) }.map_err(KmuxError::Pty)?;

        match fork_result {
            // SAFETY: we are the freshly forked child.
            ForkptyResult::Child => unsafe { plan.exec() },
            ForkptyResult::Parent { child, master } => {
                let exit_rx = reaper.watch(child);
                let io = PtyMasterIo::new(master.into_raw_fd()).map_err(|e| {
                    // No handle will own this child, so nothing else would end it.
                    signal_group(child, Signal::SIGKILL);
                    KmuxError::Io(e)
                })?;
                Ok(Self {
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
        Ok(Self {
            io,
            pid,
            exit_rx,
            size,
            keep_alive: AtomicBool::new(false),
        })
    }
}

impl Drop for PtyProcess {
    /// Kill the child's process group unless it has already exited or
    /// keep-alive is set. The reaper (or, for an inherited child, its new
    /// parent) collects the exit; nothing here waits.
    ///
    /// Dropping closes this handle's master fd. With keep-alive set the child
    /// keeps its terminal through another copy of the master that is still
    /// open: the one a handoff successor received over `SCM_RIGHTS`, or, inside
    /// a VT worker, the daemon's own.
    fn drop(&mut self) {
        if self.keep_alive.load(Ordering::Relaxed) {
            return;
        }
        // An exited child's pid may already be reaped and reused.
        if !self.is_exited() {
            signal_group(self.pid, Signal::SIGKILL);
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

        let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
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

    /// Dropping a handle to a live child kills it (and the reaper collects
    /// it). The child ignores `SIGHUP`, so the hangup from the master closing
    /// cannot do the killing in Drop's place. Needs a real child (R7).
    #[tokio::test]
    async fn dropping_a_process_kills_its_child() {
        use tokio::io::AsyncReadExt;

        let script = "trap '' HUP; echo ready; exec sleep 600";
        let mut pty =
            PtyProcess::spawn(&PtyConfig::new("/bin/sh").args(["-c", script])).expect("spawn");
        let mut seen = String::new();
        let mut buf = [0u8; 64];
        while !seen.contains("ready") {
            let n = pty.io.read(&mut buf).await.expect("read");
            assert_ne!(n, 0, "EOF before ready: {seen:?}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let pid = pty.pid;
        drop(pty);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        assert!(crate::fixtures::wait_until_dead(pid, deadline).await);
    }

    /// A pane sees its terminal and nothing else: not another pane's master,
    /// and not an fd some other part of the process opened without
    /// close-on-exec (the pipe below plays that careless library). Needs a
    /// real child to list its own fds (R7).
    #[tokio::test]
    async fn a_pane_inherits_no_fd_but_its_terminal() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        const INHERITED: i32 = 10;

        let other = PtyProcess::spawn(&PtyConfig::new("/bin/cat")).expect("spawn");
        let (read_end, _write_end) = nix::unistd::pipe().expect("pipe");
        // SAFETY: F_DUPFD (not F_DUPFD_CLOEXEC) yields a fresh, inheritable
        // fd this test owns; 200 keeps it clear of the child's own fds and
        // under macOS's default limit of 256.
        let leaky = unsafe {
            OwnedFd::from_raw_fd(nix::libc::fcntl(
                read_end.as_raw_fd(),
                nix::libc::F_DUPFD,
                200,
            ))
        };
        assert!(leaky.as_raw_fd() >= 200, "precondition: the dup succeeded");

        let output = crate::oneshot::run(&PtyConfig::new("/bin/ls").args(["/dev/fd"]))
            .await
            .expect("run");
        let fds: Vec<i32> = output
            .stdout_str()
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect();

        // 0-2 are the terminal. Whatever `ls` opens to do the listing takes
        // the lowest free numbers, just past them; anything far above was
        // inherited. The leaky fd is far above; the other master usually is
        // too, but not always (parallel tests free low numbers for reuse), so
        // `io`'s close-on-exec test is what covers it whatever its number.
        assert!(
            fds.contains(&0) && fds.contains(&2),
            "not a listing: {fds:?}"
        );
        assert!(
            fds.iter().all(|&fd| fd < INHERITED),
            "fds leaked into the pane: {fds:?} (other master {}, leaky {})",
            other.master_fd(),
            leaky.as_raw_fd()
        );
    }

    /// Each child changes directory itself, so spawns racing each other each
    /// land in their own directory, and the spawning process never moves.
    /// Needs real children to report where they run (R7).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_spawns_each_run_in_their_own_directory() {
        let before = std::env::current_dir().expect("cwd");
        let dirs: Vec<tempfile::TempDir> = (0..8)
            .map(|_| tempfile::tempdir().expect("tempdir"))
            .collect();

        let runs: Vec<_> = dirs
            .iter()
            .map(|dir| {
                let config = PtyConfig::new("/bin/pwd").cwd(dir.path());
                tokio::spawn(async move { crate::oneshot::run(&config).await })
            })
            .collect();
        for (dir, run) in dirs.iter().zip(runs) {
            let output = run.await.expect("join").expect("run");
            let expected = dir.path().canonicalize().expect("canonical");
            assert_eq!(output.stdout_str().trim(), expected.to_string_lossy());
        }
        assert_eq!(std::env::current_dir().expect("cwd"), before);
    }

    /// The daemon ignores `SIGPIPE` (the Rust runtime does), and an ignored
    /// disposition survives `execve`. Reset in the child, `yes` dies of the
    /// closed pipe silently instead of reporting a write error. Needs a real
    /// child (R7).
    #[tokio::test]
    async fn a_pipeline_whose_reader_exits_ends_quietly() {
        let config = PtyConfig::new("/bin/sh").args(["-c", "yes | head -1"]);
        let output = crate::oneshot::run(&config).await.expect("run");
        assert_eq!(output.stdout_str().trim_end(), "y");
        assert_eq!(output.status, ExitStatus::Code(0));
    }

    /// A child that cannot change directory, or cannot run its program, says
    /// why on the terminal and exits 127. Needs a real child (R7).
    #[tokio::test]
    async fn a_child_that_cannot_start_says_why_and_exits_127() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cases = [
            (
                PtyConfig::new("/bin/pwd").cwd(tmp.path().join("missing")),
                "kmux: cannot chdir to ",
            ),
            (
                PtyConfig::new("/nonexistent/program"),
                "kmux: cannot exec /nonexistent/program: errno ",
            ),
        ];
        for (config, diagnostic) in cases {
            let output = crate::oneshot::run(&config).await.expect("run");
            let text = output.stdout_str();
            assert!(text.starts_with(diagnostic), "{text:?}");
            assert_eq!(output.status, ExitStatus::Code(127), "{text:?}");
        }
    }

    /// Verify that setting `keep_alive` prevents the Drop impl from sending
    /// SIGKILL: the child process should still be running after the
    /// `PtyProcess` is dropped with `keep_alive = true`. As in a handoff, a
    /// copy of the master stays open elsewhere — without one, closing the last
    /// master would hang the terminal up.
    #[tokio::test]
    async fn keep_alive_prevents_sigkill_on_drop() {
        let pty = PtyProcess::spawn(&sleep_config()).expect("spawn failed");
        let pid = pty.pid;
        let _successors_copy = pty.io.dup_owned().expect("dup");

        pty.set_keep_alive(true);
        drop(pty);

        // A negative claim needs a window: a regression that spawned a kill
        // task from Drop would run within it, and `wait_until_dead` returns as
        // soon as the child is gone, so only the passing case waits it out.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        let died = crate::fixtures::wait_until_dead(pid, deadline).await;
        assert!(!died, "process should still be alive after keep_alive drop");

        // Clean up: kill the process ourselves.
        let _ = nix::sys::signal::kill(pid, Signal::SIGKILL);
    }
}
