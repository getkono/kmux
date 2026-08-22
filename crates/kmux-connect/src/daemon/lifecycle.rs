use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use kmux_sys::dirs::Dirs;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{DaemonStatus, query_daemon_at};

/// Maximum bytes of each log file to include in a failure error.
const BOOT_LOG_TAIL_MAX: u64 = 8 * 1024;

/// The `KMUX_KMUXD` override, read once at the environment boundary.
///
/// Every function below takes the override as a parameter instead of reading it
/// itself, so a test can pin a stub `kmuxd` without mutating the process
/// environment (docs/testing.md R3). Returns the raw value; validity (`is_file`)
/// is decided by [`find_server_binary_with`], so a stale override still falls
/// through to discovery.
fn kmuxd_override() -> Option<PathBuf> {
    std::env::var_os("KMUX_KMUXD").map(PathBuf::from)
}

/// Remove daemon artifacts only when no process owns the PID-file lock.
///
/// Handles three cases:
///
/// A held lock proves an active daemon owns the PID file. In that case the
/// socket is preserved and automatic startup fails safely instead of making the
/// existing listener unreachable or signalling an unverified PID.
pub(super) fn cleanup_stale_daemon_in(dirs: &Dirs) -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd;

    let pid_path = dirs.pid_path()?;
    let socket_path = dirs.socket_path()?;

    if pid_path.exists() {
        let pid_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pid_path)
            .map_err(|e| anyhow::anyhow!("failed to inspect {}: {e}", pid_path.display()))?;
        #[allow(deprecated)]
        match nix::fcntl::flock(
            pid_file.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EWOULDBLOCK) => {
                let owner = read_pid_file(&pid_path).map_or_else(
                    || "an active process".to_string(),
                    |pid| format!("PID {pid}"),
                );
                return Err(anyhow::anyhow!(
                    "{owner} owns the daemon PID file but the control socket is unresponsive; \
                     automatic startup left it untouched. Inspect `kmux daemon status` and \
                     `kmux daemon logs`, then run `kmux daemon restart` if needed"
                ));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "failed to lock {} while checking daemon ownership: {error}",
                    pid_path.display()
                ));
            }
        }
        std::fs::remove_file(&pid_path)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", pid_path.display()))?;
    }

    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .map_err(|e| anyhow::anyhow!("failed to remove {}: {e}", socket_path.display()))?;
    }
    Ok(())
}

/// Path to the file that captures kmuxd's stdout+stderr across a spawn attempt.
/// Delegates to the shared [`Dirs::boot_log_path`] so every daemon-spawn site
/// (here, `probe-or-start`, the handoff successor) agrees.
fn boot_log_path_in(dirs: &Dirs) -> anyhow::Result<PathBuf> {
    dirs.boot_log_path()
}

/// Spawn `kmuxd` with [`kmux_protocol::control_rpc::DAEMON_BOOT_ARGS`]
/// (`--daemon --bind 0.0.0.0 --port 0`).
///
/// The server binary handles double-fork daemonization internally via `--daemon`.
/// We use `LOCK_EX | LOCK_NB` on `daemon.spawn.lock` (a dedicated client-side
/// lock file, *not* `daemon.pid`) to prevent concurrent starts. The pid file
/// itself is owned by kmuxd's `daemonize` grandchild, which flocks it from a
/// process the client cannot observe — sharing one file with the client would
/// race the client's drop against the grandchild's flock and surface as
/// `EWOULDBLOCK` ("unable to lock pid file, errno 35").
///
/// kmuxd's stdout and stderr are redirected to `kmuxd-boot.log` in the runtime
/// dir so a crash before it becomes ready (e.g. linker error, bind failure) is
/// visible to the caller instead of silently vanishing into `/dev/null`.
///
/// Returns:
/// - `Ok(Some(child))` after a successful spawn — the caller should retain the
///   handle so it can `try_wait()` to detect pre-daemonization crashes and reap
///   the double-fork's top-level process without leaking a zombie.
/// - `Ok(None)` if another process already holds the spawn lock (the caller
///   should just poll `query_daemon()`).
/// - `Err(_)` for any other failure (bad binary, filesystem error, …).
pub(crate) fn start_daemon_in(
    dirs: &Dirs,
    kmuxd: Option<&Path>,
) -> anyhow::Result<Option<std::process::Child>> {
    use std::os::unix::fs::OpenOptionsExt;

    cleanup_stale_daemon_in(dirs)?;

    // Acquire a non-blocking exclusive flock on the spawn lock to serialize
    // concurrent kmux invocations that all try to start a daemon at once.
    let spawn_lock_path = dirs.spawn_lock_path()?;
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&spawn_lock_path)?;

    // LOCK_EX | LOCK_NB: fail immediately if another process holds the lock.
    #[allow(deprecated)]
    use nix::fcntl::{FlockArg, flock};
    use std::os::unix::io::AsRawFd;
    #[allow(deprecated)]
    if flock(lock_file.as_raw_fd(), FlockArg::LockExclusiveNonblock)
        == Err(nix::errno::Errno::EWOULDBLOCK)
    {
        // Another process is in the middle of starting a daemon — let the
        // caller retry query_daemon() rather than starting a second one.
        return Ok(None);
    }

    // Resolve the server binary path. In development `kmuxd` is a
    // sibling binary; in an installed layout it must be on PATH.
    let server_bin = find_server_binary_with(kmuxd)?;

    // Capture kmuxd's stdio so a crash during boot leaves a trail.
    let log_path = boot_log_path_in(dirs)?;
    let log_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| anyhow::anyhow!("failed to open boot log {}: {e}", log_path.display()))?;
    let stderr_file = log_file.try_clone()?;

    let child = std::process::Command::new(&server_bin)
        .args(kmux_protocol::control_rpc::DAEMON_BOOT_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", server_bin.display()))?;

    // lock_file is dropped at end of scope, releasing the spawn lock.
    Ok(Some(child))
}

/// Read the tail of the boot log and format it as an error suffix.
///
/// Returns `""` when the log is missing or empty. Capped at
/// `BOOT_LOG_TAIL_MAX` bytes so a runaway log doesn't overwhelm the error.
pub(super) fn format_boot_log_hint() -> String {
    let Ok(dirs) = Dirs::from_env() else {
        return String::new();
    };
    format_boot_log_hint_in(&dirs)
}

/// [`format_boot_log_hint`] against an explicit [`Dirs`].
fn format_boot_log_hint_in(dirs: &Dirs) -> String {
    let Ok(path) = boot_log_path_in(dirs) else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    if len == 0 {
        return String::new();
    }
    let start = len.saturating_sub(BOOT_LOG_TAIL_MAX);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    // Not `fs::read`: the seek above is the point — this reads only the tail of
    // a log that may be arbitrarily large, and `fs::read` would start over at
    // byte zero and pull the whole file into memory.
    #[expect(
        clippy::verbose_file_reads,
        reason = "reads from a seek offset, which fs::read cannot express"
    )]
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let tail = String::from_utf8_lossy(&bytes);
    let trimmed = tail.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    format!(
        "\n\n--- kmuxd output (tail of {}): ---\n{trimmed}",
        path.display()
    )
}

/// Return the current byte length of the daemon log, or 0 if it does not exist.
///
/// Call this before spawning so `format_daemon_log_tail` can show only the new
/// entries written by this particular spawn attempt.
fn daemon_log_size_in(dirs: &Dirs) -> u64 {
    dirs.daemon_log_path()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map_or(0, |m| m.len())
}

/// Read daemon.log from `from_offset` to the end and format it as an error suffix.
///
/// Only shows content written after `from_offset` so old runtime log entries
/// from a previous daemon instance do not appear in startup failure messages.
fn format_daemon_log_tail_in(dirs: &Dirs, from_offset: u64) -> String {
    let Ok(path) = dirs.daemon_log_path() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    if len <= from_offset {
        return String::new();
    }
    let read_from = if len - from_offset > BOOT_LOG_TAIL_MAX {
        len - BOOT_LOG_TAIL_MAX
    } else {
        from_offset
    };
    if file.seek(SeekFrom::Start(read_from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    // Not `fs::read`: the seek above is the point — this reads only the tail of
    // a log that may be arbitrarily large, and `fs::read` would start over at
    // byte zero and pull the whole file into memory.
    #[expect(
        clippy::verbose_file_reads,
        reason = "reads from a seek offset, which fs::read cannot express"
    )]
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let tail = String::from_utf8_lossy(&bytes);
    let trimmed = tail.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    format!("\n\n--- kmuxd daemon log (new since start): ---\n{trimmed}")
}

/// Ensure exactly one local daemon is running and return its connection params.
///
/// Fast path: if a daemon is already responding, return immediately.
/// Slow path: start a new daemon and poll until it responds (up to 5 seconds).
///
/// While polling, we also `try_wait()` on the spawned child: a non-zero exit
/// before the daemon goes live is a hard failure and we surface it immediately
/// with the captured stdio, instead of waiting out the full timeout. A clean
/// `exit(0)` is normal after the double-fork daemonization completes.
pub async fn ensure_daemon() -> anyhow::Result<DaemonStatus> {
    ensure_daemon_in(&Dirs::from_env()?, kmuxd_override().as_deref()).await
}

/// [`ensure_daemon`] against an explicit [`Dirs`] and an explicit `kmuxd`
/// override (the `KMUX_KMUXD` value, or `None`).
///
/// This is the whole implementation; [`ensure_daemon`] only resolves the two
/// environment inputs. A test builds `Dirs::rooted(tmp)` and points `kmuxd` at a
/// stub binary, so nothing about it depends on process-global state.
pub async fn ensure_daemon_in(dirs: &Dirs, kmuxd: Option<&Path>) -> anyhow::Result<DaemonStatus> {
    let socket = dirs.socket_path()?;

    // Fast path — existing daemon.
    if let Some(status) = query_daemon_at(&socket).await {
        return Ok(status);
    }

    // Snapshot daemon log position so we only surface entries written by
    // this spawn attempt, not leftover lines from a previous daemon run.
    let daemon_log_offset = daemon_log_size_in(dirs);

    // Slow path — start a new daemon. `None` means another process is already
    // starting one; we just poll in that case.
    let mut spawned = start_daemon_in(dirs, kmuxd)?;
    let mut ever_spawned = spawned.is_some();

    // Poll until the daemon is ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut retry_start = false;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(status) = query_daemon_at(&socket).await {
            return Ok(status);
        }

        // Detect and surface a crash that happened before kmuxd daemonized.
        if let Some(child) = spawned.as_mut()
            && let Ok(Some(exit)) = child.try_wait()
        {
            if !exit.success() {
                return Err(anyhow::anyhow!(
                    "kmuxd exited with status {exit} before becoming ready{}",
                    format_boot_log_hint_in(dirs)
                ));
            }
            // exit(0) → daemonize()'s top-level process completed normally;
            // the grandchild is now detached. Stop tracking the handle.
            spawned = None;
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        // After 3 s with no response, try to clean up and restart once.
        if !retry_start && tokio::time::Instant::now() >= (deadline - Duration::from_secs(2)) {
            retry_start = true;
            match start_daemon_in(dirs, kmuxd) {
                Ok(Some(s)) => {
                    spawned = Some(s);
                    ever_spawned = true;
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }
    }

    // Build a diagnostic hint: boot log (stderr captured during spawn) plus
    // any new daemon.log lines written by the grandchild after daemonizing.
    let boot_hint = format_boot_log_hint_in(dirs);
    let daemon_hint = if ever_spawned {
        format_daemon_log_tail_in(dirs, daemon_log_offset)
    } else {
        String::new()
    };
    let hint = format!("{boot_hint}{daemon_hint}");

    if ever_spawned && !hint.is_empty() {
        // The binary was found and started — the logs tell the user what went
        // wrong, so skip the misleading PATH suggestion.
        Err(anyhow::anyhow!("local daemon failed to start{hint}"))
    } else {
        Err(anyhow::anyhow!(
            "timed out waiting for local daemon to start; \
             check that kmuxd is on PATH or in the same directory as kmux{hint}"
        ))
    }
}

pub fn pid_alive(pid: u32) -> bool {
    // kill(pid, 0) returns Ok if the process exists and we can signal it.
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

pub(super) fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// Return the PID of a daemon that is *alive* per its pid file, regardless of
/// whether it still answers on the control socket.
///
/// Backs the "unresponsive daemon" branch of `kmux daemon stop`: when
/// [`query_daemon`](super::query_daemon) returns `None` but a live process owns
/// the pid file, the control socket is wedged and the only way out is an OS
/// signal. Returns `None` when there is no pid file, it is unparseable, or the
/// PID it names is already dead.
pub fn running_daemon_pid() -> Option<u32> {
    running_daemon_pid_in(&Dirs::from_env().ok()?)
}

/// [`running_daemon_pid`] against an explicit [`Dirs`].
pub fn running_daemon_pid_in(dirs: &Dirs) -> Option<u32> {
    let pid_path = dirs.pid_path().ok()?;
    let pid = read_pid_file(&pid_path)?;
    pid_alive(pid).then_some(pid)
}

/// Poll until `pid` is no longer alive or `timeout` elapses.
///
/// Returns `true` the moment the process is gone, `false` if it is still alive
/// when the deadline passes. This is the verification step that stops
/// `kmux daemon stop` from ever reporting success for a daemon that is wedged
/// mid-shutdown — the previous behaviour trusted the daemon's `"ok"` reply,
/// which is sent *before* the process actually exits.
pub async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Force a daemon process to exit using OS signals, then verify it is gone.
///
/// `term_first` controls the escalation:
/// - `true` (unresponsive daemon): send `SIGTERM`, give it `grace` to exit on
///   its own, then `SIGKILL` if still alive.
/// - `false` (a responsive daemon that already received a graceful `stop` but
///   would not exit): skip straight to `SIGKILL` — re-asking for a graceful
///   shutdown it is already ignoring would only waste `grace`.
///
/// `ESRCH` (no such process) is treated as success: the daemon is already gone.
/// After a confirmed kill the stale pid/socket files are swept. Returns `Err`
/// when the process is still alive after `SIGKILL` (e.g. uninterruptible sleep,
/// or `EPERM`), so the caller never reports a kill that did not happen.
///
/// `nix` signalling is the platform-agnostic primitive across kmux's supported
/// targets (macOS + Linux). Automatic startup never uses this path; it relies
/// on the PID-file lock in `cleanup_stale_daemon` instead.
pub async fn force_kill_daemon(pid: u32, term_first: bool, grace: Duration) -> anyhow::Result<()> {
    force_kill_daemon_in(&Dirs::from_env()?, pid, term_first, grace).await
}

/// [`force_kill_daemon`] against an explicit [`Dirs`] — the sweep of the stale
/// pid/socket files happens under `dirs`, so a test never touches the real
/// runtime dir.
pub async fn force_kill_daemon_in(
    dirs: &Dirs,
    pid: u32,
    term_first: bool,
    grace: Duration,
) -> anyhow::Result<()> {
    let nix_pid = Pid::from_raw(pid as i32);

    if term_first {
        match kill(nix_pid, Signal::SIGTERM) {
            Ok(()) => {}
            Err(nix::errno::Errno::ESRCH) => {
                let _ = cleanup_stale_daemon_in(dirs);
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to SIGTERM daemon (PID {pid}): {e}"));
            }
        }
        if wait_for_exit(pid, grace).await {
            let _ = cleanup_stale_daemon_in(dirs);
            return Ok(());
        }
    }

    match kill(nix_pid, Signal::SIGKILL) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {
            let _ = cleanup_stale_daemon_in(dirs);
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!("failed to SIGKILL daemon (PID {pid}): {e}"));
        }
    }

    // SIGKILL cannot be caught, but process teardown is asynchronous — give the
    // kernel a brief, bounded window to reap it before we verify.
    if wait_for_exit(pid, Duration::from_secs(2)).await {
        let _ = cleanup_stale_daemon_in(dirs);
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "daemon (PID {pid}) is still alive after SIGKILL; it may be stuck in \
             uninterruptible sleep — inspect it manually"
        ))
    }
}

pub(crate) fn find_server_binary() -> anyhow::Result<PathBuf> {
    find_server_binary_with(kmuxd_override().as_deref())
}

/// [`find_server_binary`] with the `KMUX_KMUXD` override passed in rather than
/// read from the environment, so a test can exercise each precedence step
/// without mutating process-global state.
pub(crate) fn find_server_binary_with(override_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    // 0. Explicit override (dev workflows, unusual layouts). Mirrors `KMUX_BIN`
    //    (diagnostic emitter) and `KMUX_APP` (macOS bundle). Honored only when it
    //    points at a real file, so a stale value falls through to discovery
    //    rather than hard-failing.
    if let Some(candidate) = override_path
        && candidate.is_file()
    {
        return Ok(candidate.to_path_buf());
    }

    // 1. Same directory as the running executable (typical installed layout).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("kmuxd");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // 2. Debug builds: prefer the matching `target/<profile>/kmuxd` over any
    //    release kmuxd on PATH. A debug client that spawned a release daemon
    //    would never see it (release writes its socket under `kmux/`, the debug
    //    client polls `kmux-debug/`), so without this a `cargo run` / `swift run`
    //    GUI silently picks up `~/.cargo/bin/kmuxd` and times out. The dir is
    //    baked at build time from the crate's `OUT_DIR` (see `build.rs`).
    #[cfg(debug_assertions)]
    if let Some(dir) = option_env!("KMUXD_TARGET_DIR") {
        let candidate = Path::new(dir).join("kmuxd");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    // 3. PATH lookup.
    if let Ok(path) = which_server() {
        return Ok(path);
    }

    Err(anyhow::anyhow!(
        "could not find kmuxd binary; ensure it is installed alongside kmux or on \
         PATH (or set KMUX_KMUXD to its path)"
    ))
}

pub(super) fn which_server() -> anyhow::Result<PathBuf> {
    which_server_on(&std::env::var("PATH").unwrap_or_default())
}

/// [`which_server`] with the search path passed in rather than read from `PATH`.
pub(super) fn which_server_on(path_var: &str) -> anyhow::Result<PathBuf> {
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join("kmuxd");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!("kmuxd not found on PATH"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::stop_daemon_at;

    /// Every test builds its own isolated tree. There is no lock and no
    /// `set_var`: `Dirs::rooted` gives each test a private runtime/state dir, so
    /// the whole module is parallel-safe (docs/testing.md R3/R13).
    fn fixture() -> (tempfile::TempDir, Dirs) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());
        (tmp, dirs)
    }

    /// Block until `path` can actually be `exec`d, then return.
    ///
    /// Writing a file and immediately exec-ing it is racy in a *multi-threaded*
    /// process: if another thread forks while our write fd is still open (the
    /// sibling tests here spawn `sleep`/`true`), the child inherits a writable
    /// descriptor for the same inode until it reaches `exec`, and `execve`
    /// answers `ETXTBSY` for that window. Nothing reopens the file for writing
    /// afterwards, so one successful exec proves the window has closed for good.
    ///
    /// Serialising the module behind a lock used to hide this; the fix belongs
    /// in the test that creates an executable, not in a global lock.
    fn await_executable(path: &Path) {
        for _ in 0..200 {
            match std::process::Command::new(path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    return;
                }
                Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ETXTBSY as i32) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("stub {} is not executable: {error}", path.display()),
            }
        }
        panic!("stub {} stayed ETXTBSY for 2s", path.display());
    }

    #[tokio::test]
    async fn query_nonexistent_socket_returns_none() {
        let (_tmp, dirs) = fixture();
        let socket = dirs.socket_path().expect("socket path");
        // No socket exists — should return None quickly.
        assert!(query_daemon_at(&socket).await.is_none());
    }

    #[tokio::test]
    async fn query_stale_socket_returns_none() {
        let (_tmp, dirs) = fixture();
        // Create the runtime dir and a dummy socket file (nothing listening).
        let _ = dirs.runtime_dir().expect("runtime dir");
        let socket_path = dirs.socket_path().expect("socket path");
        // Create an empty file at the socket path so the path "exists" but
        // no one is listening — connect will fail.
        std::fs::write(&socket_path, b"").expect("write stale socket");
        assert!(query_daemon_at(&socket_path).await.is_none());
    }

    #[test]
    fn cleanup_preserves_files_owned_by_an_active_daemon_lock() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let (_tmp, dirs) = fixture();
        let pid_path = dirs.pid_path().expect("pid path");
        let socket_path = dirs.socket_path().expect("socket path");
        std::fs::write(&pid_path, std::process::id().to_string()).expect("write pid file");
        std::fs::write(&socket_path, b"socket placeholder").expect("write socket placeholder");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&pid_path)
            .expect("open pid file");
        #[allow(deprecated)]
        nix::fcntl::flock(
            held.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .expect("take the pid-file lock");

        let error = cleanup_stale_daemon_in(&dirs).expect_err("a held lock must refuse cleanup");
        let msg = error.to_string();
        assert!(msg.contains("left it untouched"), "{msg}");
        assert!(
            msg.contains(&format!("PID {}", std::process::id())),
            "the refusal must name the process that owns the pid file: {msg}"
        );
        assert!(pid_path.exists());
        assert!(socket_path.exists());

        drop(held);
    }

    #[test]
    fn cleanup_removes_unlocked_stale_artifacts() {
        let (_tmp, dirs) = fixture();
        let pid_path = dirs.pid_path().expect("pid path");
        let socket_path = dirs.socket_path().expect("socket path");
        std::fs::write(&pid_path, "stale").expect("write pid file");
        std::fs::write(&socket_path, b"socket placeholder").expect("write socket placeholder");

        cleanup_stale_daemon_in(&dirs).expect("an unlocked pid file is stale and must be swept");
        assert!(!pid_path.exists());
        assert!(!socket_path.exists());
    }

    #[test]
    fn start_daemon_returns_none_when_spawn_lock_held() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let (_tmp, dirs) = fixture();

        let lock_path = dirs.spawn_lock_path().expect("spawn lock path");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .expect("open spawn lock");
        #[allow(deprecated)]
        nix::fcntl::flock(
            held.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .expect("take the spawn lock");

        // Spawn lock contended → start_daemon must short-circuit to Ok(None)
        // without spawning kmuxd or touching the pid file.
        let result = start_daemon_in(&dirs, None).expect("expected Ok, got Err");
        assert!(
            result.is_none(),
            "expected Ok(None) when spawn lock is held"
        );

        let pid_path = dirs.pid_path().expect("pid path");
        assert!(
            !pid_path.exists(),
            "start_daemon must not touch daemon.pid when contended"
        );

        drop(held);
    }

    #[tokio::test]
    async fn stop_daemon_not_running_returns_err() {
        let (_tmp, dirs) = fixture();
        let socket = dirs.socket_path().expect("socket path");
        // No socket present — stop should return an error.
        let error = stop_daemon_at(&socket)
            .await
            .expect_err("expected error when daemon is not running");
        assert!(
            error.to_string().contains("daemon is not running"),
            "the error must say why the stop failed: {error}"
        );
    }

    #[test]
    fn daemon_log_size_returns_zero_for_missing_file() {
        let (_tmp, dirs) = fixture();
        // No daemon.log — should not panic, should return 0.
        assert_eq!(daemon_log_size_in(&dirs), 0);
    }

    #[test]
    fn daemon_log_size_reports_the_current_length() {
        let (_tmp, dirs) = fixture();
        let log_path = dirs.daemon_log_path().expect("daemon log path");
        std::fs::write(&log_path, b"twelve bytes").expect("write daemon log");
        assert_eq!(
            daemon_log_size_in(&dirs),
            12,
            "the snapshot offset must be the real byte length, or the tail would \
             replay old lines"
        );
    }

    #[test]
    fn format_daemon_log_tail_empty_when_no_new_content() {
        let (_tmp, dirs) = fixture();

        let log_path = dirs.daemon_log_path().expect("daemon log path");
        std::fs::write(&log_path, b"old line\n").expect("write daemon log");
        let offset = std::fs::metadata(&log_path).expect("stat daemon log").len();

        // Nothing written after the offset — should return empty.
        assert_eq!(format_daemon_log_tail_in(&dirs, offset), String::new());
    }

    #[test]
    fn format_daemon_log_tail_returns_only_new_content() {
        let (_tmp, dirs) = fixture();

        let log_path = dirs.daemon_log_path().expect("daemon log path");
        std::fs::write(&log_path, b"old log line\n").expect("write daemon log");
        let offset = std::fs::metadata(&log_path).expect("stat daemon log").len();

        // Append a new line after the snapshot.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("append to daemon log");
        writeln!(f, "Error: failed to bind port 8443: address already in use")
            .expect("write new line");

        let tail = format_daemon_log_tail_in(&dirs, offset);
        assert!(
            tail.contains("failed to bind port 8443"),
            "should include new daemon log line: {tail}"
        );
        assert!(
            !tail.contains("old log line"),
            "must not include pre-snapshot content: {tail}"
        );
        assert!(
            tail.contains("daemon log"),
            "should label the section: {tail}"
        );
    }

    #[test]
    fn boot_log_hint_is_empty_without_a_boot_log_and_quotes_it_otherwise() {
        let (_tmp, dirs) = fixture();
        assert_eq!(
            format_boot_log_hint_in(&dirs),
            String::new(),
            "no boot log means no suffix at all, not an empty header"
        );

        let path = boot_log_path_in(&dirs).expect("boot log path");
        std::fs::write(&path, b"error while loading shared libraries\n").expect("write boot log");
        let hint = format_boot_log_hint_in(&dirs);
        assert!(
            hint.contains("error while loading shared libraries"),
            "the hint must carry kmuxd's captured output: {hint}"
        );
        assert!(
            hint.contains(&path.display().to_string()),
            "the hint must name the file it quoted: {hint}"
        );
    }

    /// Simulates the exact regression we once hit: kmuxd crashes before it can
    /// daemonize (e.g. missing `.so`) and the client should surface stderr,
    /// not a generic timeout.
    #[tokio::test]
    async fn ensure_daemon_surfaces_crash_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, dirs) = fixture();

        // Point the resolver straight at a stub "kmuxd" that prints a crash
        // message and exits non-zero without ever binding a socket. Passing the
        // override explicitly keeps this deterministic regardless of whether a
        // real `target/<profile>/kmuxd` happens to be built — which the
        // debug-only fallback in `find_server_binary` would otherwise prefer
        // over a stub planted on `$PATH`.
        let fake_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&fake_dir).expect("create stub dir");
        let fake_kmuxd = fake_dir.join("kmuxd");
        std::fs::write(
            &fake_kmuxd,
            "#!/bin/sh\n\
             echo 'error while loading shared libraries: libkmux_ghostty.so' >&2\n\
             exit 127\n",
        )
        .expect("write stub kmuxd");
        std::fs::set_permissions(&fake_kmuxd, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub kmuxd");
        await_executable(&fake_kmuxd);

        let result = ensure_daemon_in(&dirs, Some(&fake_kmuxd)).await;

        let err = result.expect_err("fake kmuxd should not produce a live daemon");
        let msg = err.to_string();
        assert!(
            msg.contains("libkmux_ghostty.so"),
            "error must include captured stderr tail: {msg}"
        );
        assert!(
            msg.contains("kmuxd output"),
            "error must label the captured output section: {msg}"
        );
    }

    /// The `KMUX_KMUXD` override is the highest-precedence resolution step: when
    /// it points at a real file it must win over the exe-sibling, debug
    /// `target/<profile>`, and `$PATH` lookups — this is what lets the dev GUI
    /// tasks pin the debug `target/debug/kmuxd` instead of an installed release
    /// one on `$PATH`.
    #[test]
    fn kmux_kmuxd_override_takes_precedence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let override_bin = tmp.path().join("my-kmuxd");
        std::fs::write(&override_bin, b"#!/bin/sh\n").expect("write override binary");

        assert_eq!(
            find_server_binary_with(Some(&override_bin))
                .expect("override file exists, so resolution must succeed"),
            override_bin,
            "the override must win over sibling / target-dir / PATH resolution",
        );
    }

    /// A stale override (pointing at a non-existent path) must be ignored and
    /// never handed back, so resolution falls through to real discovery.
    #[test]
    fn stale_kmux_kmuxd_override_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("does-not-exist-kmuxd");

        if let Ok(path) = find_server_binary_with(Some(&missing)) {
            assert_ne!(
                path, missing,
                "a non-existent override must never be returned"
            );
        }
    }

    #[test]
    fn which_server_scans_the_path_in_order_and_reports_a_miss() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let empty = tmp.path().join("empty");
        let filled = tmp.path().join("filled");
        std::fs::create_dir_all(&empty).expect("create empty dir");
        std::fs::create_dir_all(&filled).expect("create filled dir");
        let kmuxd = filled.join("kmuxd");
        std::fs::write(&kmuxd, b"#!/bin/sh\n").expect("write kmuxd");

        let path_var = format!("{}:{}", empty.display(), filled.display());
        assert_eq!(
            which_server_on(&path_var).expect("kmuxd is on the search path"),
            kmuxd,
            "the first PATH entry holding a kmuxd wins"
        );

        let error = which_server_on(&empty.display().to_string())
            .expect_err("an empty search path must not resolve");
        assert!(error.to_string().contains("not found on PATH"), "{error}");
    }

    #[tokio::test]
    async fn wait_for_exit_times_out_for_live_pid() {
        // A long-lived child stays alive past the deadline → wait_for_exit must
        // report `false` (still running), never a spurious success.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(!wait_for_exit(pid, Duration::from_millis(300)).await);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn wait_for_exit_returns_true_once_pid_dies() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait(); // reap so the PID is fully gone, as init would
        assert!(wait_for_exit(pid, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn force_kill_daemon_terminates_a_live_process() {
        // cleanup_stale_daemon (called on success) touches the runtime dir, so
        // pin it to an isolated tree like the other daemon tests.
        let (_tmp, dirs) = fixture();

        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        // In production the daemon is reparented to init, which reaps it the
        // instant it dies. Here the test process is the parent, so reap in the
        // background to free the PID rather than leaving a zombie (which would
        // still answer kill(pid, 0) and look "alive").
        let reaper = std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });

        force_kill_daemon_in(&dirs, pid, true, Duration::from_secs(1))
            .await
            .expect("force kill should terminate and verify the process");
        reaper.join().expect("reaper thread");
        assert!(
            !pid_alive(pid),
            "the process must be gone once we report success"
        );
    }

    #[tokio::test]
    async fn force_kill_daemon_is_ok_when_already_gone() {
        let (_tmp, dirs) = fixture();

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let _ = child.wait(); // already exited and reaped → kill yields ESRCH

        force_kill_daemon_in(&dirs, pid, true, Duration::from_millis(100))
            .await
            .expect("killing an already-dead PID is success (ESRCH)");
    }

    #[test]
    fn running_daemon_pid_reports_live_and_absent() {
        let (_tmp, dirs) = fixture();

        // No pid file yet → None. (pid_path() materializes the runtime dir.)
        let pid_path = dirs.pid_path().expect("pid path");
        assert!(running_daemon_pid_in(&dirs).is_none());

        // Our own PID is alive → reported back.
        std::fs::write(&pid_path, std::process::id().to_string()).expect("write pid file");
        assert_eq!(running_daemon_pid_in(&dirs), Some(std::process::id()));

        // Unparseable pid file → None, never a panic.
        std::fs::write(&pid_path, "not-a-pid").expect("write pid file");
        assert!(running_daemon_pid_in(&dirs).is_none());
    }
}
