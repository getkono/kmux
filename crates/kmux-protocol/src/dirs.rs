use std::path::PathBuf;

use nix::unistd::getuid;

/// Subdirectory name appended to `$XDG_RUNTIME_DIR`, `$XDG_STATE_HOME`, and
/// their respective fallback roots to isolate the two profiles.
///
/// Debug builds use `kmux-debug` so a `cargo run` instance can coexist with an
/// installed release instance on the same machine without colliding on sockets,
/// PID files, logs, metrics, or session checkpoints.
///
/// Config files (`$XDG_CONFIG_HOME/kmux/` and `$XDG_CONFIG_HOME/kmuxd/`) are
/// intentionally *shared* across profiles — they represent user intent, not
/// runtime state.
#[cfg(debug_assertions)]
pub const KMUX_DIR_NAME: &str = "kmux-debug";
#[cfg(not(debug_assertions))]
pub const KMUX_DIR_NAME: &str = "kmux";

/// Cargo build profile of a kmux binary.
///
/// Advertised by `kmuxd` in its status response and checked by `kmux` during
/// the control-socket handshake: a mismatch means the client and the daemon
/// resolved different runtime dirs (`kmux-debug/` vs `kmux/`), so the client
/// would have silently attached to the wrong instance — we refuse.
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildProfile {
    Debug,
    Release,
}

impl BuildProfile {
    /// Profile the current crate was compiled with.
    #[cfg(debug_assertions)]
    pub const CURRENT: Self = Self::Debug;
    #[cfg(not(debug_assertions))]
    pub const CURRENT: Self = Self::Release;

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Release => "release",
        }
    }
}

impl std::fmt::Display for BuildProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Returns the kmux runtime directory, creating it if necessary.
///
/// Prefers `$XDG_RUNTIME_DIR/{KMUX_DIR_NAME}` — a per-user, in-memory
/// directory set by systemd/logind on Linux with tight permissions (mode
/// 0700).
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
    let dir = base.join(KMUX_DIR_NAME);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create runtime dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the daemon Unix domain socket (control channel).
pub fn socket_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// Path to the daemon Unix domain socket for data connections.
///
/// Distinct from `socket_path()` (the control socket) — this socket accepts
/// full client sessions using the same framing protocol as TCP/QUIC.
pub fn data_socket_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon-data.sock"))
}

/// Path to the daemon PID file.
pub fn pid_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.pid"))
}

/// Path to the auth token file.
pub fn token_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("token"))
}

/// Returns the kmux configuration directory, creating it if necessary.
///
/// Uses `$XDG_CONFIG_HOME/kmux` per the XDG Base Directory Specification, falling back to
/// `$HOME/.config/kmux` when `XDG_CONFIG_HOME` is unset.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(val) => PathBuf::from(val),
        Err(_) => {
            let home = std::env::var("HOME").map(PathBuf::from).or_else(|_| {
                nix::unistd::User::from_uid(getuid())
                    .ok()
                    .flatten()
                    .map(|u| u.dir)
                    .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
            })?;
            home.join(".config")
        }
    };
    let dir = base.join("kmux");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create config dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the TOFU known-hosts file used for TLS certificate pinning.
pub fn known_hosts_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("known_hosts.toml"))
}

/// Directory where cached TLS certificates are stored (e.g. auto-generated self-signed certs).
pub fn tls_cert_dir() -> anyhow::Result<PathBuf> {
    let dir = config_dir()?.join("tls");
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create TLS cert dir {}: {e}", dir.display()))?;
    Ok(dir)
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
    let dir = base.join(KMUX_DIR_NAME);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create state dir {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Path to the client log file (appended to by all kmux instances).
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

/// Path to the rolling JSONL file where client metrics samples are appended.
///
/// Shared across concurrent `kmux` processes via advisory file locking
/// (`flock`); see `kmux_client::metrics::jsonl::JsonlSink`.
pub fn metrics_log_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("metrics.jsonl"))
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
        assert_eq!(dir, tmp.path().join(KMUX_DIR_NAME));
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
        assert_eq!(dir, tmp.path().join(KMUX_DIR_NAME));
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
    fn metrics_log_path_in_state_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", tmp.path()) };
        let path = metrics_log_path().unwrap();
        assert!(path.ends_with("metrics.jsonl"));
        assert!(path.parent().unwrap().ends_with(KMUX_DIR_NAME));
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
