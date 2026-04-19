use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{DaemonStatus, query_daemon};

/// Maximum bytes of each log file to include in a failure error.
const BOOT_LOG_TAIL_MAX: u64 = 8 * 1024;

/// Remove stale daemon artifacts (zombie prevention).
///
/// Handles three cases:
/// 1. PID file exists, PID is dead → remove pid file.
/// 2. PID file exists, PID is alive but socket is unresponsive → kill the zombie.
/// 3. Socket file exists but connect fails → remove stale socket file.
pub(super) fn cleanup_stale_daemon() {
    let pid_path = match kmux_protocol::dirs::pid_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    let socket_path = match kmux_protocol::dirs::socket_path() {
        Ok(p) => p,
        Err(_) => return,
    };

    if pid_path.exists() {
        if let Some(pid) = read_pid_file(&pid_path) {
            if pid_alive(pid) {
                // Process is alive but not responding on the socket — kill it.
                let nix_pid = Pid::from_raw(pid as i32);
                let _ = kill(nix_pid, Signal::SIGTERM);
                // Give it a moment to exit, then SIGKILL if still alive.
                std::thread::sleep(Duration::from_millis(500));
                if pid_alive(pid) {
                    let _ = kill(nix_pid, Signal::SIGKILL);
                }
            }
            // PID is dead (or we just killed it) — remove the pid file.
            let _ = std::fs::remove_file(&pid_path);
        } else {
            // Can't parse pid file — remove it.
            let _ = std::fs::remove_file(&pid_path);
        }
    }

    // Remove stale socket if present.
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }
}

/// Path to the file that captures kmuxd's stdout+stderr across a spawn attempt.
///
/// Lives in the runtime dir alongside the socket/pid file so the user can tail
/// it manually, and so we can include its contents in a startup-failure error.
fn boot_log_path() -> anyhow::Result<PathBuf> {
    Ok(kmux_protocol::dirs::runtime_dir()?.join("kmuxd-boot.log"))
}

/// Spawn `kmuxd --daemon --self-signed --bind 127.0.0.1 --port 0`.
///
/// The server binary handles double-fork daemonization internally via `--daemon`.
/// We use `LOCK_EX | LOCK_NB` on the pid file to prevent concurrent starts.
///
/// kmuxd's stdout and stderr are redirected to `kmuxd-boot.log` in the runtime
/// dir so a crash before it becomes ready (e.g. linker error, bind failure) is
/// visible to the caller instead of silently vanishing into `/dev/null`.
///
/// Returns:
/// - `Ok(Some(child))` after a successful spawn — the caller should retain the
///   handle so it can `try_wait()` to detect pre-daemonization crashes and reap
///   the double-fork's top-level process without leaking a zombie.
/// - `Ok(None)` if another process already holds the pid-file lock (the caller
///   should just poll `query_daemon()`).
/// - `Err(_)` for any other failure (bad binary, filesystem error, …).
pub(crate) fn start_daemon() -> anyhow::Result<Option<std::process::Child>> {
    use std::os::unix::fs::OpenOptionsExt;

    cleanup_stale_daemon();

    // Acquire a non-blocking exclusive flock on the pid file to serialize
    // concurrent kmux invocations that all try to start a daemon at once.
    let pid_path = kmux_protocol::dirs::pid_path()?;
    let lock_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&pid_path)?;

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

    // Release the flock — the daemonized child will write its own PID file.
    // The lock_file is dropped here.
    Ok(Some(child))
}

/// Read the tail of the boot log and format it as an error suffix.
///
/// Returns `""` when the log is missing or empty. Capped at
/// `BOOT_LOG_TAIL_MAX` bytes so a runaway log doesn't overwhelm the error.
fn format_boot_log_hint() -> String {
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
            cleanup_stale_daemon();
            if let Some(s) = start_daemon().ok().flatten() {
                spawned = Some(s);
                ever_spawned = true;
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

pub(super) fn pid_alive(pid: u32) -> bool {
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

pub(crate) fn find_server_binary() -> anyhow::Result<std::path::PathBuf> {
    // 1. Same directory as the running executable (typical installed layout).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("kmuxd");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // 2. PATH lookup.
    if let Ok(path) = which_server() {
        return Ok(path);
    }

    Err(anyhow::anyhow!(
        "could not find kmuxd binary; ensure it is installed alongside kmux or on PATH"
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

        // Place a stub "kmuxd" on PATH that prints a crash message and exits
        // non-zero without ever binding a socket.
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

        let old_path = std::env::var("PATH").unwrap_or_default();
        unsafe { std::env::set_var("PATH", fake_dir.as_os_str()) };

        let result = ensure_daemon().await;

        unsafe { std::env::set_var("PATH", &old_path) };

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
}
