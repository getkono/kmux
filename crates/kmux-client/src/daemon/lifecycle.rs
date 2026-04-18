use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use super::{DaemonStatus, query_daemon};

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

/// Spawn `kmuxd --daemon --self-signed --bind 127.0.0.1 --port 0`.
///
/// The server binary handles double-fork daemonization internally via `--daemon`.
/// We use `LOCK_EX | LOCK_NB` on the pid file to prevent concurrent starts.
pub(crate) fn start_daemon() -> anyhow::Result<()> {
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
        return Err(anyhow::anyhow!(
            "another process is already starting the daemon"
        ));
    }

    // Resolve the server binary path. In development `kmuxd` is a
    // sibling binary; in an installed layout it must be on PATH.
    let server_bin = find_server_binary()?;

    std::process::Command::new(&server_bin)
        .args(kmux_protocol::control_rpc::DAEMON_BOOT_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", server_bin.display()))?;

    // Release the flock — the daemonized child will write its own PID file.
    // The lock_file is dropped here.
    Ok(())
}

/// Ensure exactly one local daemon is running and return its connection params.
///
/// Fast path: if a daemon is already responding, return immediately.
/// Slow path: start a new daemon and poll until it responds (up to 5 seconds).
pub async fn ensure_daemon() -> anyhow::Result<DaemonStatus> {
    // Fast path — existing daemon.
    if let Some(status) = query_daemon().await {
        return Ok(status);
    }

    // Slow path — start a new daemon.
    match start_daemon() {
        Ok(()) => {}
        Err(e)
            if e.to_string()
                .contains("another process is already starting") =>
        {
            // Race: another concurrent kmux is starting a daemon. Just poll.
        }
        Err(e) => return Err(e),
    }

    // Poll until the daemon is ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut retry_start = false;
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(status) = query_daemon().await {
            return Ok(status);
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }

        // After 3 s with no response, try to clean up and restart once.
        if !retry_start && tokio::time::Instant::now() >= (deadline - Duration::from_secs(2)) {
            retry_start = true;
            cleanup_stale_daemon();
            let _ = start_daemon();
        }
    }

    Err(anyhow::anyhow!(
        "timed out waiting for local daemon to start; \
         check that kmuxd is on PATH or in the same directory as kmux"
    ))
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
}
