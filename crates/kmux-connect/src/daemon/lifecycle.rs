use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{DaemonStatus, query_daemon};

/// Maximum bytes of each log file to include in a failure error.
const BOOT_LOG_TAIL_MAX: u64 = 8 * 1024;

/// Remove daemon artifacts only when no process owns the PID-file lock.
///
/// Handles three cases:
///
/// A held lock proves an active daemon owns the PID file. In that case the
/// socket is preserved and automatic startup fails safely instead of making the
/// existing listener unreachable or signalling an unverified PID.
pub(super) fn cleanup_stale_daemon() -> anyhow::Result<()> {
    use std::os::unix::io::AsRawFd;

    let pid_path = kmux_protocol::dirs::pid_path()?;
    let socket_path = kmux_protocol::dirs::socket_path()?;

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
                let owner = read_pid_file(&pid_path)
                    .map(|pid| format!("PID {pid}"))
                    .unwrap_or_else(|| "an active process".to_string());
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
/// Delegates to the shared [`kmux_protocol::dirs::boot_log_path`] so every
/// daemon-spawn site (here, `probe-or-start`, the handoff successor) agrees.
fn boot_log_path() -> anyhow::Result<PathBuf> {
    kmux_protocol::dirs::boot_log_path()
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
pub(crate) fn start_daemon() -> anyhow::Result<Option<std::process::Child>> {
    use std::os::unix::fs::OpenOptionsExt;

    cleanup_stale_daemon()?;

    // Acquire a non-blocking exclusive flock on the spawn lock to serialize
    // concurrent kmux invocations that all try to start a daemon at once.
    let spawn_lock_path = kmux_protocol::dirs::spawn_lock_path()?;
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
    if let Err(nix::errno::Errno::EWOULDBLOCK) =
        flock(lock_file.as_raw_fd(), FlockArg::LockExclusiveNonblock)
    {
        // Another process is in the middle of starting a daemon — let the
        // caller retry query_daemon() rather than starting a second one.
        return Ok(None);
    }

    // Resolve the server binary path. In development `kmuxd` is a
    // sibling binary; in an installed layout it must be on PATH.
    let server_bin = find_server_binary()?;

    // Capture kmuxd's stdio so a crash during boot leaves a trail.
    let log_path = boot_log_path()?;
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
    let Ok(path) = boot_log_path() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return String::new();
    }
    let start = len.saturating_sub(BOOT_LOG_TAIL_MAX);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
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
fn daemon_log_size() -> u64 {
    kmux_protocol::dirs::daemon_log_path()
        .ok()
        .and_then(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Read daemon.log from `from_offset` to the end and format it as an error suffix.
///
/// Only shows content written after `from_offset` so old runtime log entries
/// from a previous daemon instance do not appear in startup failure messages.
fn format_daemon_log_tail(from_offset: u64) -> String {
    let Ok(path) = kmux_protocol::dirs::daemon_log_path() else {
        return String::new();
    };
    let Ok(mut file) = std::fs::File::open(&path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
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
    // Fast path — existing daemon.
    if let Some(status) = query_daemon().await {
        return Ok(status);
    }

    // Snapshot daemon log position so we only surface entries written by
    // this spawn attempt, not leftover lines from a previous daemon run.
    let daemon_log_offset = daemon_log_size();

    // Slow path — start a new daemon. `None` means another process is already
    // starting one; we just poll in that case.
    let mut spawned = start_daemon()?;
    let mut ever_spawned = spawned.is_some();

    // Poll until the daemon is ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut retry_start = false;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(status) = query_daemon().await {
            return Ok(status);
        }

        // Detect and surface a crash that happened before kmuxd daemonized.
        if let Some(child) = spawned.as_mut()
            && let Ok(Some(exit)) = child.try_wait()
        {
            if !exit.success() {
                return Err(anyhow::anyhow!(
                    "kmuxd exited with status {exit} before becoming ready{}",
                    format_boot_log_hint()
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
            match start_daemon() {
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
    let boot_hint = format_boot_log_hint();
    let daemon_hint = if ever_spawned {
        format_daemon_log_tail(daemon_log_offset)
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

pub(super) fn read_pid_file(path: &std::path::Path) -> Option<u32> {
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
    let pid_path = kmux_protocol::dirs::pid_path().ok()?;
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
/// on the PID-file lock in [`cleanup_stale_daemon`] instead.
pub async fn force_kill_daemon(pid: u32, term_first: bool, grace: Duration) -> anyhow::Result<()> {
    let nix_pid = Pid::from_raw(pid as i32);

    if term_first {
        match kill(nix_pid, Signal::SIGTERM) {
            Ok(()) => {}
            Err(nix::errno::Errno::ESRCH) => {
                let _ = cleanup_stale_daemon();
                return Ok(());
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to SIGTERM daemon (PID {pid}): {e}"));
            }
        }
        if wait_for_exit(pid, grace).await {
            let _ = cleanup_stale_daemon();
            return Ok(());
        }
    }

    match kill(nix_pid, Signal::SIGKILL) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => {
            let _ = cleanup_stale_daemon();
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!("failed to SIGKILL daemon (PID {pid}): {e}"));
        }
    }

    // SIGKILL cannot be caught, but process teardown is asynchronous — give the
    // kernel a brief, bounded window to reap it before we verify.
    if wait_for_exit(pid, Duration::from_secs(2)).await {
        let _ = cleanup_stale_daemon();
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "daemon (PID {pid}) is still alive after SIGKILL; it may be stuck in \
             uninterruptible sleep — inspect it manually"
        ))
    }
}

pub(crate) fn find_server_binary() -> anyhow::Result<std::path::PathBuf> {
    // 0. Explicit override (dev workflows, unusual layouts). Mirrors `KMUX_BIN`
    //    (diagnostic emitter) and `KMUX_APP` (macOS bundle). Honored only when it
    //    points at a real file, so a stale env var falls through to discovery
    //    rather than hard-failing.
    if let Some(path) = std::env::var_os("KMUX_KMUXD") {
        let candidate = std::path::PathBuf::from(path);
        if candidate.is_file() {
            return Ok(candidate);
        }
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
        let candidate = std::path::Path::new(dir).join("kmuxd");
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

pub(super) fn which_server() -> anyhow::Result<std::path::PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join("kmuxd");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow::anyhow!("kmuxd not found on PATH"))
}

#[cfg(test)]
mod tests {
    use super::super::ENV_LOCK;
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // intentional: guard protects env var for the whole test
    async fn query_nonexistent_socket_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: protected by ENV_LOCK; no concurrent test mutates XDG_RUNTIME_DIR.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        // No socket exists — should return None quickly.
        assert!(query_daemon().await.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // intentional: guard protects env var for the whole test
    async fn query_stale_socket_returns_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        // Create the runtime dir and a dummy socket file (nothing listening).
        let _ = kmux_protocol::dirs::runtime_dir().unwrap();
        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        // Create an empty file at the socket path so the path "exists" but
        // no one is listening — connect will fail.
        std::fs::write(&socket_path, b"").unwrap();
        assert!(query_daemon().await.is_none());
    }

    #[test]
    fn cleanup_preserves_files_owned_by_an_active_daemon_lock() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        let pid_path = kmux_protocol::dirs::pid_path().unwrap();
        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        std::fs::write(&socket_path, b"socket placeholder").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&pid_path)
            .unwrap();
        #[allow(deprecated)]
        nix::fcntl::flock(
            held.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .unwrap();

        let error = cleanup_stale_daemon().unwrap_err();
        assert!(error.to_string().contains("left it untouched"));
        assert!(pid_path.exists());
        assert!(socket_path.exists());

        drop(held);
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[test]
    fn cleanup_removes_unlocked_stale_artifacts() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        let pid_path = kmux_protocol::dirs::pid_path().unwrap();
        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        std::fs::write(&pid_path, "stale").unwrap();
        std::fs::write(&socket_path, b"socket placeholder").unwrap();

        cleanup_stale_daemon().unwrap();
        assert!(!pid_path.exists());
        assert!(!socket_path.exists());

        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[test]
    fn start_daemon_returns_none_when_spawn_lock_held() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let lock_path = kmux_protocol::dirs::spawn_lock_path().unwrap();
        let held = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        #[allow(deprecated)]
        nix::fcntl::flock(
            held.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .unwrap();

        // Spawn lock contended → start_daemon must short-circuit to Ok(None)
        // without spawning kmuxd or touching the pid file.
        let result = start_daemon().expect("expected Ok, got Err");
        assert!(
            result.is_none(),
            "expected Ok(None) when spawn lock is held"
        );

        let pid_path = kmux_protocol::dirs::pid_path().unwrap();
        assert!(
            !pid_path.exists(),
            "start_daemon must not touch daemon.pid when contended"
        );

        drop(held);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn stop_daemon_not_running_returns_err() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        // No socket present — stop should return an error.
        let result = crate::daemon::stop_daemon().await;
        assert!(result.is_err(), "expected error when daemon is not running");
    }

    #[test]
    fn daemon_log_size_returns_zero_for_missing_file() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        // No daemon.log — should not panic, should return 0.
        assert_eq!(daemon_log_size(), 0);
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn format_daemon_log_tail_empty_when_no_new_content() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };

        let log_path = kmux_protocol::dirs::daemon_log_path().unwrap();
        std::fs::write(&log_path, b"old line\n").unwrap();
        let offset = std::fs::metadata(&log_path).unwrap().len();

        // Nothing written after the offset — should return empty.
        assert_eq!(format_daemon_log_tail(offset), String::new());
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn format_daemon_log_tail_returns_only_new_content() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };

        let log_path = kmux_protocol::dirs::daemon_log_path().unwrap();
        std::fs::write(&log_path, b"old log line\n").unwrap();
        let offset = std::fs::metadata(&log_path).unwrap().len();

        // Append a new line after the snapshot.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(f, "Error: failed to bind port 8443: address already in use").unwrap();

        let tail = format_daemon_log_tail(offset);
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
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    /// Simulates the exact regression we just hit: kmuxd crashes before it can
    /// daemonize (e.g. missing `.so`) and the client should surface stderr,
    /// not a generic timeout.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ensure_daemon_surfaces_crash_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        // Point the resolver straight at a stub "kmuxd" that prints a crash
        // message and exits non-zero without ever binding a socket. Using
        // `KMUX_KMUXD` (highest precedence) keeps this deterministic regardless
        // of whether a real `target/<profile>/kmuxd` happens to be built — which
        // the debug-only fallback in `find_server_binary` would otherwise prefer
        // over a stub planted on `$PATH`.
        let fake_dir = tmp.path().join("bin");
        std::fs::create_dir_all(&fake_dir).unwrap();
        let fake_kmuxd = fake_dir.join("kmuxd");
        std::fs::write(
            &fake_kmuxd,
            "#!/bin/sh\n\
             echo 'error while loading shared libraries: libkmux_ghostty.so' >&2\n\
             exit 127\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_kmuxd, std::fs::Permissions::from_mode(0o755)).unwrap();

        unsafe { std::env::set_var("KMUX_KMUXD", &fake_kmuxd) };

        let result = ensure_daemon().await;

        unsafe { std::env::remove_var("KMUX_KMUXD") };

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

    /// `KMUX_KMUXD` is the highest-precedence resolution step: when it points at
    /// a real file it must win over the exe-sibling, debug `target/<profile>`,
    /// and `$PATH` lookups — this is what lets the dev GUI tasks pin the debug
    /// `target/debug/kmuxd` instead of an installed release one on `$PATH`.
    #[test]
    fn kmux_kmuxd_override_takes_precedence() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let override_bin = tmp.path().join("my-kmuxd");
        std::fs::write(&override_bin, b"#!/bin/sh\n").unwrap();

        unsafe { std::env::set_var("KMUX_KMUXD", &override_bin) };
        let resolved = find_server_binary();
        unsafe { std::env::remove_var("KMUX_KMUXD") };

        assert_eq!(
            resolved.expect("override file exists, so resolution must succeed"),
            override_bin,
            "KMUX_KMUXD must win over sibling / target-dir / PATH resolution",
        );
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
    #[allow(clippy::await_holding_lock)]
    async fn force_kill_daemon_terminates_a_live_process() {
        // cleanup_stale_daemon (called on success) touches the runtime dir, so
        // pin it to a tempdir like the other daemon tests.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

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

        force_kill_daemon(pid, true, Duration::from_secs(1))
            .await
            .expect("force kill should terminate and verify the process");
        reaper.join().unwrap();

        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn force_kill_daemon_is_ok_when_already_gone() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        let _ = child.wait(); // already exited and reaped → kill yields ESRCH

        force_kill_daemon(pid, true, Duration::from_millis(100))
            .await
            .expect("killing an already-dead PID is success (ESRCH)");

        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    #[test]
    fn running_daemon_pid_reports_live_and_absent() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        // No pid file yet → None. (pid_path() materializes the runtime dir.)
        let pid_path = kmux_protocol::dirs::pid_path().unwrap();
        assert!(running_daemon_pid().is_none());

        // Our own PID is alive → reported back.
        std::fs::write(&pid_path, std::process::id().to_string()).unwrap();
        assert_eq!(running_daemon_pid(), Some(std::process::id()));

        // Unparseable pid file → None, never a panic.
        std::fs::write(&pid_path, "not-a-pid").unwrap();
        assert!(running_daemon_pid().is_none());

        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
    }

    /// A stale `KMUX_KMUXD` (pointing at a non-existent path) must be ignored and
    /// never handed back, so resolution falls through to real discovery.
    #[test]
    fn stale_kmux_kmuxd_override_is_ignored() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist-kmuxd");

        unsafe { std::env::set_var("KMUX_KMUXD", &missing) };
        let resolved = find_server_binary();
        unsafe { std::env::remove_var("KMUX_KMUXD") };

        if let Ok(path) = resolved {
            assert_ne!(
                path, missing,
                "a non-existent override must never be returned"
            );
        }
    }
}
