use std::path::PathBuf;

use nix::unistd::getuid;

/// Returns the kmux runtime directory, creating it if necessary.
///
/// Prefers `$XDG_RUNTIME_DIR/kmux` — a per-user, in-memory directory set by
/// systemd/logind on Linux with tight permissions (mode 0700).
///
/// Falls back to `/tmp/kmux-<uid>` when `XDG_RUNTIME_DIR` is unset (macOS,
/// BSDs, containers, minimal Linux environments). The UID suffix prevents
/// cross-user collisions, and `/tmp` is guaranteed on all POSIX systems.
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            let uid = getuid().as_raw();
            PathBuf::from(format!("/tmp/kmux-{uid}"))
        }
    };
    let dir = base.join("kmux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create runtime dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the daemon Unix domain socket.
pub fn socket_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// Path to the daemon PID file.
pub fn pid_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.pid"))
}

/// Path to the auth token file.
pub fn token_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("token"))
}

/// Returns the kmux state directory for persistent data (logs, etc.), creating it if necessary.
///
/// Uses `$XDG_STATE_HOME/kmux` per the XDG Base Directory Specification, falling back to
/// `$HOME/.local/state/kmux` when `XDG_STATE_HOME` is unset.
pub fn state_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            let home = std::env::var("HOME").map(PathBuf::from).or_else(|_| {
                nix::unistd::User::from_uid(getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.dir)
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
            })?;
            home.join(".local").join("state")
        }
    };
    let dir = base.join("kmux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create state dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the client log file (appended to by all kmux/kmux-gui instances).
pub fn client_log_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("client.log"))
}

/// Path to the daemon log file (appended to by kmuxd).
pub fn daemon_log_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("daemon.log"))
}

/// Path to the per-connection log file for the given instance ID.
///
/// Each client startup writes its connection metadata here upon successful authentication.
pub fn connection_log_path(instance_id: &str) -> anyhow::Result<PathBuf> {
    let dir = state_dir()?.join("connections");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create connections dir {}: {e}", dir.display()))?;
    Ok(dir.join(format!("{instance_id}.log")))
}

/// Returns the directory where session state is persisted, creating it if necessary.
///
/// Lives under the state directory: `$XDG_STATE_HOME/kmux/sessions/`.
pub fn sessions_dir() -> anyhow::Result<PathBuf> {
    let dir = state_dir()?.join("sessions");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create sessions dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the daemon session state file used for persistence across restarts.
pub fn session_state_path() -> anyhow::Result<PathBuf> {
    Ok(sessions_dir()?.join("state.bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests mutate XDG_RUNTIME_DIR, which is global state.
    /// Run them under a mutex so they don't race each other or other tests
    /// in this crate that also call set_var.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn runtime_dir_xdg() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: protected by ENV_LOCK; no other thread touches XDG_RUNTIME_DIR
        // while this guard is held.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        let dir = runtime_dir().unwrap();
        assert_eq!(dir, tmp.path().join("kmux"));
        assert!(dir.exists());
    }

    #[test]
    fn path_helpers() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        assert!(socket_path().unwrap().ends_with("daemon.sock"));
        assert!(pid_path().unwrap().ends_with("daemon.pid"));
        assert!(token_path().unwrap().ends_with("token"));
    }

    #[test]
    fn state_dir_xdg() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        let dir = state_dir().unwrap();
        assert_eq!(dir, tmp.path().join("kmux"));
        assert!(dir.exists());
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn state_path_helpers() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        assert!(client_log_path().unwrap().ends_with("client.log"));
        assert!(daemon_log_path().unwrap().ends_with("daemon.log"));
        let conn_path = connection_log_path("abc123ef").unwrap();
        assert!(conn_path.ends_with("abc123ef.log"));
        assert!(conn_path.parent().unwrap().ends_with("connections"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn sessions_dir_created() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        let dir = sessions_dir().unwrap();
        assert!(dir.exists());
        assert!(dir.ends_with("sessions"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn session_state_path_is_in_sessions_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        let path = session_state_path().unwrap();
        assert!(path.ends_with("state.bin"));
        assert!(path.parent().unwrap().ends_with("sessions"));
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}
