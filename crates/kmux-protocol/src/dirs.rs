use std::path::{Path, PathBuf};

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
/// Falls back to `/tmp/kmux-<uid>/{KMUX_DIR_NAME}` when `XDG_RUNTIME_DIR` is
/// unset (macOS, BSDs, containers, minimal Linux environments). The UID parent
/// and profile directory are both verified as non-symlink, user-owned `0700`
/// directories before any socket, PID, or token path is returned.
pub fn runtime_dir() -> anyhow::Result<PathBuf> {
    let (base, secure_base) = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(val) => (PathBuf::from(val), false),
        Err(_) => {
            let uid = getuid().as_raw();
            (PathBuf::from(format!("/tmp/kmux-{uid}")), true)
        }
    };
    create_runtime_dir(&base, secure_base)
}

fn create_runtime_dir(base: &Path, secure_base: bool) -> anyhow::Result<PathBuf> {
    if secure_base {
        create_private_dir(base)?;
    } else if !base.is_dir() {
        return Err(anyhow::anyhow!(
            "runtime base {} is not an existing directory",
            base.display()
        ));
    }
    let dir = base.join(KMUX_DIR_NAME);
    create_private_dir(&dir)?;
    Ok(dir)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(anyhow::anyhow!(
                "failed to create runtime dir {}: {error}",
                dir.display()
            ));
        }
    }
    validate_private_dir(dir)
}

#[cfg(unix)]
fn validate_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| anyhow::anyhow!("failed to stat runtime dir {}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(anyhow::anyhow!(
            "runtime dir {} must not be a symlink",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(anyhow::anyhow!(
            "runtime path {} is not a directory",
            dir.display()
        ));
    }
    let uid = getuid().as_raw();
    if meta.uid() != uid {
        return Err(anyhow::anyhow!(
            "runtime dir {} is owned by uid {}, expected {}",
            dir.display(),
            meta.uid(),
            uid
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| anyhow::anyhow!("failed to chmod runtime dir {}: {e}", dir.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir(dir).or_else(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(error)
        }
    })?;
    Ok(())
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

/// Path to the daemon handoff Unix domain socket.
///
/// Created transiently by an outgoing daemon during a graceful restart so the
/// incoming daemon can pull live PTY master file descriptors across via
/// `SCM_RIGHTS`. Distinct from the control and data sockets so that the two
/// daemons can overlap without contending for those fixed paths. See
/// [`super::control_rpc::HANDOFF_PROTOCOL_VERSION`] and `docs/daemon-handoff.md`.
pub fn handoff_socket_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("handoff.sock"))
}

/// Path to the client-side spawn lock.
///
/// `kmux-client` flocks this file (LOCK_EX | LOCK_NB) to gate concurrent
/// `kmux` invocations from racing to spawn a daemon. Distinct from
/// `pid_path()` because `daemonize` also flocks the pid file from inside
/// kmuxd's grandchild — sharing one file would self-deadlock.
pub fn spawn_lock_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.spawn.lock"))
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

/// Path to the Ed25519 machine/user identity keypair (issue #146).
///
/// Lives in the config dir (PKCS#8 DER, mode 0600), alongside the TOFU store and
/// cached TLS certs. Like those, it is *shared* across the debug/release profiles
/// — it represents the stable identity of this user@machine, not runtime state,
/// so a `cargo run` instance and an installed release present the same identity.
pub fn identity_key_path() -> anyhow::Result<PathBuf> {
    Ok(config_dir()?.join("identity.key"))
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

/// Path to the file that captures a freshly-spawned kmuxd's stdout+stderr
/// before it daemonizes and switches to [`daemon_log_path`].
///
/// Lives in the runtime dir next to the socket/pid file. Every path that spawns
/// a daemon — the client auto-spawn, `kmuxd probe-or-start`, and the
/// graceful-restart successor — redirects the child here, so a boot crash
/// (linker error, bind failure, full disk) leaves a trail instead of vanishing
/// into `/dev/null`. `kmux daemon restart` reads its tail to explain a timeout.
pub fn boot_log_path() -> anyhow::Result<PathBuf> {
    Ok(runtime_dir()?.join("kmuxd-boot.log"))
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

/// Path to the daemon's frame-trace JSONL (issue #72 diagnostics).
///
/// Written by `kmuxd` when `KMUX_FRAME_TRACE` is set — one
/// [`crate::trace::DaemonDiffRecord`] per emitted diff. Consumed by the
/// `kmux debug tearing` analyzer.
pub fn daemon_trace_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("frame_trace_daemon.jsonl"))
}

/// Path to the client's frame-trace JSONL (issue #72 diagnostics).
///
/// Written by the `kmux` client when `KMUX_FRAME_TRACE` is set — one
/// [`crate::trace::ClientTickRecord`] per pump tick. Consumed by the
/// `kmux debug tearing` analyzer.
pub fn client_trace_path() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("frame_trace_client.jsonl"))
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

/// Path to the closed-session graveyard file (issue #64).
///
/// A sibling of [`session_state_path`], kept separate so that the large,
/// immutable closed-session snapshots are rewritten only when the graveyard set
/// changes — never on the periodic live checkpoint.
pub fn closed_sessions_path() -> anyhow::Result<PathBuf> {
    Ok(sessions_dir()?.join("closed.bin"))
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

    #[cfg(unix)]
    #[test]
    fn fallback_runtime_parent_must_not_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let redirected = tmp.path().join("redirected");
        std::fs::create_dir(&redirected).unwrap();
        let fallback = tmp.path().join("kmux-1234");
        symlink(&redirected, &fallback).unwrap();

        let error = create_runtime_dir(&fallback, true).unwrap_err();
        assert!(error.to_string().contains("must not be a symlink"));
        assert!(!redirected.join(KMUX_DIR_NAME).exists());
    }

    #[test]
    fn path_helpers() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        assert!(socket_path().unwrap().ends_with("daemon.sock"));
        assert!(pid_path().unwrap().ends_with("daemon.pid"));
        assert!(spawn_lock_path().unwrap().ends_with("daemon.spawn.lock"));
        assert!(token_path().unwrap().ends_with("token"));
    }

    /// Debug and release builds must resolve *different* runtime dirs, or a debug
    /// client could attach to a release daemon (and vice-versa) over a shared
    /// socket. The whole client↔daemon contract relies on both sides computing
    /// the same profile-namespaced path here, so pin the namespace constant and
    /// confirm the data-plane sockets are namespaced by it.
    #[test]
    fn runtime_paths_are_profile_isolated() {
        // The test binary is built in debug, so the constant resolves to the
        // debug namespace; release builds use the bare `kmux` namespace.
        #[cfg(debug_assertions)]
        assert_eq!(KMUX_DIR_NAME, "kmux-debug");
        #[cfg(not(debug_assertions))]
        assert_eq!(KMUX_DIR_NAME, "kmux");
        assert_ne!("kmux", "kmux-debug", "the two profiles must never collide");

        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };
        // Control + data sockets both sit under the profile-namespaced dir.
        for p in [socket_path().unwrap(), data_socket_path().unwrap()] {
            assert!(
                p.starts_with(tmp.path().join(KMUX_DIR_NAME)),
                "{} is not namespaced by {KMUX_DIR_NAME}",
                p.display()
            );
        }
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
