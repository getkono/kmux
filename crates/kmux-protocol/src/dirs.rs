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

/// The three XDG base directories kmux resolves every path from.
///
/// This type is the seam that keeps path resolution out of the process
/// environment. [`Dirs::from_env`] is the only place in the workspace that reads
/// `XDG_*`/`HOME`; [`Dirs::rooted`] builds an isolated tree for a test. Before it
/// existed, isolating a test meant unsafely overwriting the XDG variables under
/// a hand-rolled mutex — process-global state that forced tests to run serially
/// and that a per-file lock could not actually protect from another file's
/// tests. See docs/testing.md R3.
///
/// Construction is cheap and does no I/O; directories are created on demand by
/// the accessors, exactly as the free functions have always done.
#[derive(Debug, Clone)]
pub struct Dirs {
    runtime_base: PathBuf,
    /// True when kmux owns the runtime base and must create it `0700` (the
    /// `/tmp/kmux-<uid>` fallback). False when the base belongs to someone else
    /// (`$XDG_RUNTIME_DIR`, created by systemd/logind), where kmux only creates
    /// the profile directory inside it.
    runtime_base_is_ours: bool,
    config_base: PathBuf,
    state_base: PathBuf,
}

impl Dirs {
    /// Resolve the bases from the environment.
    ///
    /// The only reader of `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`
    /// and `HOME` in the workspace. Resolve once, near the process entry point,
    /// and pass the value down.
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |k: &str| std::env::var(k).ok();
        Self::resolve(
            var("XDG_RUNTIME_DIR"),
            var("XDG_CONFIG_HOME"),
            var("XDG_STATE_HOME"),
            getuid().as_raw(),
            || {
                std::env::var("HOME").map(PathBuf::from).or_else(|_| {
                    nix::unistd::User::from_uid(getuid())
                        .ok()
                        .flatten()
                        .map(|u| u.dir)
                        .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))
                })
            },
        )
    }

    /// The resolution rules, with the environment passed in.
    ///
    /// Split out from [`Dirs::from_env`] so the rules — XDG wins over the
    /// fallback, the `/tmp` fallback is uid-scoped and owned by us, config is
    /// `~/.config` while state is `~/.local/state` — are testable without
    /// mutating the process environment. `from_env` is then a thin reader with
    /// nothing left to get wrong.
    ///
    /// `home` is a closure and not a value so it is consulted only when a base
    /// actually falls back to it: a process that needs the runtime dir alone is
    /// unaffected by an unresolvable home directory, matching the behaviour of
    /// the free functions before this type existed.
    fn resolve(
        xdg_runtime: Option<String>,
        xdg_config: Option<String>,
        xdg_state: Option<String>,
        uid: u32,
        home: impl Fn() -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<Self> {
        let (runtime_base, runtime_base_is_ours) = match xdg_runtime {
            Some(val) => (PathBuf::from(val), false),
            // No XDG_RUNTIME_DIR (macOS, BSDs, containers, minimal Linux). Fall
            // back to a uid-scoped dir under /tmp that we own and must lock down.
            None => (PathBuf::from(format!("/tmp/kmux-{uid}")), true),
        };
        let config_base = match xdg_config {
            Some(val) => PathBuf::from(val),
            None => home()?.join(".config"),
        };
        let state_base = match xdg_state {
            Some(val) => PathBuf::from(val),
            None => home()?.join(".local").join("state"),
        };

        Ok(Self {
            runtime_base,
            runtime_base_is_ours,
            config_base,
            state_base,
        })
    }

    /// Bases under a single `root`, for tests and any caller with explicit paths.
    ///
    /// Pure — nothing is created until an accessor is called. The runtime base is
    /// marked as ours so it gets the same `0700` creation and validation the
    /// `/tmp` fallback receives, which keeps the permission logic on the tested
    /// path rather than only on the production one.
    pub fn rooted(root: &Path) -> Self {
        Self {
            runtime_base: root.join("run"),
            runtime_base_is_ours: true,
            config_base: root.join("config"),
            state_base: root.join("state"),
        }
    }

    /// The kmux runtime directory, creating it if necessary.
    ///
    /// Prefers `$XDG_RUNTIME_DIR/{KMUX_DIR_NAME}` — a per-user, in-memory
    /// directory set by systemd/logind on Linux with tight permissions (mode
    /// 0700). Falls back to `/tmp/kmux-<uid>/{KMUX_DIR_NAME}`; the UID parent and
    /// profile directory are both verified as non-symlink, user-owned `0700`
    /// directories before any socket, PID, or token path is returned.
    pub fn runtime_dir(&self) -> anyhow::Result<PathBuf> {
        create_runtime_dir(&self.runtime_base, self.runtime_base_is_ours)
    }

    /// The kmux configuration directory, creating it if necessary.
    ///
    /// `$XDG_CONFIG_HOME/kmux`, falling back to `$HOME/.config/kmux`. Note the
    /// literal `kmux`, not [`KMUX_DIR_NAME`]: config is shared across the debug
    /// and release profiles because it represents user intent, not runtime state.
    pub fn config_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.config_base.join("kmux");
        create_dir_all_named(&dir, "config dir")?;
        Ok(dir)
    }

    /// The kmux state directory for persistent data (logs, session state),
    /// creating it if necessary.
    ///
    /// `$XDG_STATE_HOME/{KMUX_DIR_NAME}`, falling back to
    /// `$HOME/.local/state/{KMUX_DIR_NAME}`.
    pub fn state_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.state_base.join(KMUX_DIR_NAME);
        create_dir_all_named(&dir, "state dir")?;
        Ok(dir)
    }

    // ── Runtime-dir paths ────────────────────────────────────────────────────

    /// The daemon Unix domain socket (control channel).
    pub fn socket_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("daemon.sock"))
    }

    /// The daemon Unix domain socket for data connections.
    ///
    /// Distinct from [`Dirs::socket_path`] (the control socket) — this socket
    /// accepts full client sessions using the same framing protocol as TCP/QUIC.
    pub fn data_socket_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("daemon-data.sock"))
    }

    /// The daemon PID file.
    pub fn pid_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("daemon.pid"))
    }

    /// The daemon handoff Unix domain socket.
    ///
    /// Created transiently by an outgoing daemon during a graceful restart so the
    /// incoming daemon can pull live PTY master file descriptors across via
    /// `SCM_RIGHTS`. Distinct from the control and data sockets so the two daemons
    /// can overlap without contending for those fixed paths. See
    /// [`super::control_rpc::HANDOFF_PROTOCOL_VERSION`] and `docs/daemon-handoff.md`.
    pub fn handoff_socket_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("handoff.sock"))
    }

    /// The client-side spawn lock.
    ///
    /// `kmux-connect` flocks this file (LOCK_EX | LOCK_NB) to gate concurrent
    /// `kmux` invocations from racing to spawn a daemon. Distinct from
    /// [`Dirs::pid_path`] because `daemonize` also flocks the pid file from inside
    /// kmuxd's grandchild — sharing one file would self-deadlock.
    pub fn spawn_lock_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("daemon.spawn.lock"))
    }

    /// The auth token file.
    pub fn token_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("token"))
    }

    /// The file capturing a freshly-spawned kmuxd's stdout+stderr before it
    /// daemonizes and switches to [`Dirs::daemon_log_path`].
    ///
    /// Lives in the runtime dir next to the socket/pid file. Every path that
    /// spawns a daemon — the client auto-spawn, `kmuxd probe-or-start`, and the
    /// graceful-restart successor — redirects the child here, so a boot crash
    /// (linker error, bind failure, full disk) leaves a trail instead of vanishing
    /// into `/dev/null`. `kmux daemon restart` reads its tail to explain a timeout.
    pub fn boot_log_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.runtime_dir()?.join("kmuxd-boot.log"))
    }

    // ── Config-dir paths ─────────────────────────────────────────────────────

    /// The TOFU known-hosts file used for TLS certificate pinning.
    pub fn known_hosts_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.config_dir()?.join("known_hosts.toml"))
    }

    /// The Ed25519 machine/user identity keypair (issue #146).
    ///
    /// Lives in the config dir (PKCS#8 DER, mode 0600), alongside the TOFU store
    /// and cached TLS certs. Like those, it is *shared* across the debug/release
    /// profiles — it represents the stable identity of this user@machine, not
    /// runtime state, so a `cargo run` instance and an installed release present
    /// the same identity.
    pub fn identity_key_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.config_dir()?.join("identity.key"))
    }

    /// Directory holding cached TLS certificates (e.g. auto-generated
    /// self-signed certs), creating it if necessary.
    pub fn tls_cert_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.config_dir()?.join("tls");
        create_dir_all_named(&dir, "TLS cert dir")?;
        Ok(dir)
    }

    // ── State-dir paths ──────────────────────────────────────────────────────

    /// The client log file (appended to by all kmux instances).
    pub fn client_log_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.state_dir()?.join("client.log"))
    }

    /// The daemon log file (appended to by kmuxd).
    pub fn daemon_log_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.state_dir()?.join("daemon.log"))
    }

    /// The per-connection log file for `instance_id`, creating the containing
    /// directory if necessary.
    ///
    /// Each client startup writes its connection metadata here upon successful
    /// authentication.
    pub fn connection_log_path(&self, instance_id: &str) -> anyhow::Result<PathBuf> {
        let dir = self.state_dir()?.join("connections");
        create_dir_all_named(&dir, "connections dir")?;
        Ok(dir.join(format!("{instance_id}.log")))
    }

    /// The rolling JSONL file where client metrics samples are appended.
    ///
    /// Shared across concurrent `kmux` processes via advisory file locking
    /// (`flock`); see `kmux_client::metrics::jsonl::JsonlSink`.
    pub fn metrics_log_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.state_dir()?.join("metrics.jsonl"))
    }

    /// The daemon's frame-trace JSONL (issue #72 diagnostics).
    ///
    /// Written by `kmuxd` when `KMUX_FRAME_TRACE` is set — one
    /// [`crate::trace::DaemonDiffRecord`] per emitted diff. Consumed by the
    /// `kmux debug tearing` analyzer.
    pub fn daemon_trace_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.state_dir()?.join("frame_trace_daemon.jsonl"))
    }

    /// The client's frame-trace JSONL (issue #72 diagnostics).
    ///
    /// Written by the `kmux` client when `KMUX_FRAME_TRACE` is set — one
    /// [`crate::trace::ClientTickRecord`] per pump tick. Consumed by the
    /// `kmux debug tearing` analyzer.
    pub fn client_trace_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.state_dir()?.join("frame_trace_client.jsonl"))
    }

    /// The directory where session state is persisted, creating it if necessary.
    pub fn sessions_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = self.state_dir()?.join("sessions");
        create_dir_all_named(&dir, "sessions dir")?;
        Ok(dir)
    }

    /// The daemon session state file used for persistence across restarts.
    pub fn session_state_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.sessions_dir()?.join("state.bin"))
    }

    /// The closed-session graveyard file (issue #64).
    ///
    /// A sibling of [`Dirs::session_state_path`], kept separate so the large,
    /// immutable closed-session snapshots are rewritten only when the graveyard
    /// set changes — never on the periodic live checkpoint.
    pub fn closed_sessions_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.sessions_dir()?.join("closed.bin"))
    }
}

fn create_dir_all_named(dir: &Path, what: &str) -> anyhow::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(dir)
        .map_err(|e| anyhow::anyhow!("failed to create {what} {}: {e}", dir.display()))
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

// ── Environment-resolved convenience wrappers ────────────────────────────────
//
// Each resolves [`Dirs::from_env`] and delegates. They are the original API and
// remain the right choice at a leaf call site that no test needs to isolate —
// `kmuxd`'s startup, the CLI subcommands, the trace writers. Threading a `Dirs`
// value all the way down to those would be pure diff with no assertion behind
// it. Code whose tests *do* need isolation should take a `&Dirs` parameter
// instead; see docs/testing.md R3.

macro_rules! env_wrappers {
    ($($(#[$m:meta])* $name:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {
        $(
            $(#[$m])*
            pub fn $name($($arg: $ty),*) -> anyhow::Result<PathBuf> {
                Dirs::from_env()?.$name($($arg),*)
            }
        )*
    };
}

env_wrappers! {
    /// Returns the kmux runtime directory, creating it if necessary.
    runtime_dir(),
    /// Returns the kmux configuration directory, creating it if necessary.
    config_dir(),
    /// Returns the kmux state directory, creating it if necessary.
    state_dir(),
    /// Path to the daemon Unix domain socket (control channel).
    socket_path(),
    /// Path to the daemon Unix domain socket for data connections.
    data_socket_path(),
    /// Path to the daemon PID file.
    pid_path(),
    /// Path to the daemon handoff Unix domain socket.
    handoff_socket_path(),
    /// Path to the client-side spawn lock.
    spawn_lock_path(),
    /// Path to the auth token file.
    token_path(),
    /// Path to the captured stdout+stderr of a freshly-spawned kmuxd.
    boot_log_path(),
    /// Path to the TOFU known-hosts file used for TLS certificate pinning.
    known_hosts_path(),
    /// Path to the Ed25519 machine/user identity keypair (issue #146).
    identity_key_path(),
    /// Directory where cached TLS certificates are stored.
    tls_cert_dir(),
    /// Path to the client log file.
    client_log_path(),
    /// Path to the daemon log file.
    daemon_log_path(),
    /// Path to the per-connection log file for the given instance ID.
    connection_log_path(instance_id: &str),
    /// Path to the rolling JSONL file where client metrics samples are appended.
    metrics_log_path(),
    /// Path to the daemon's frame-trace JSONL (issue #72 diagnostics).
    daemon_trace_path(),
    /// Path to the client's frame-trace JSONL (issue #72 diagnostics).
    client_trace_path(),
    /// Returns the directory where session state is persisted.
    sessions_dir(),
    /// Path to the daemon session state file used for persistence across restarts.
    session_state_path(),
    /// Path to the closed-session graveyard file (issue #64).
    closed_sessions_path(),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test here builds its own isolated tree. There is no lock and no
    /// `set_var`: `Dirs::rooted` is the whole reason this module no longer
    /// mutates process-global state to test itself (docs/testing.md R3/R13).
    fn fixture_dirs() -> (tempfile::TempDir, Dirs) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());
        (tmp, dirs)
    }

    #[test]
    fn runtime_dir_is_namespaced_by_profile_under_the_runtime_base() {
        let (tmp, dirs) = fixture_dirs();
        let dir = dirs.runtime_dir().expect("runtime dir");
        assert_eq!(dir, tmp.path().join("run").join(KMUX_DIR_NAME));
        assert!(dir.exists());
    }

    fn home() -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from("/home/ada"))
    }

    #[test]
    fn xdg_runtime_dir_wins_and_is_not_ours_to_create() {
        let d =
            Dirs::resolve(Some("/run/user/1000".into()), None, None, 1000, home).expect("resolves");
        assert_eq!(d.runtime_base, PathBuf::from("/run/user/1000"));
        assert!(
            !d.runtime_base_is_ours,
            "the XDG base belongs to systemd/logind; creating or chmod-ing it is not ours to do"
        );
    }

    #[test]
    fn missing_xdg_runtime_dir_falls_back_to_a_uid_scoped_tmp_dir_we_own() {
        let d = Dirs::resolve(None, None, None, 4242, home).expect("resolves");
        assert_eq!(
            d.runtime_base,
            PathBuf::from("/tmp/kmux-4242"),
            "the fallback must be uid-scoped or two users would share a socket dir"
        );
        assert!(
            d.runtime_base_is_ours,
            "we create the /tmp fallback, so it must be validated and chmod 0700"
        );
    }

    #[test]
    fn config_and_state_fall_back_to_the_xdg_defaults_under_home() {
        let d = Dirs::resolve(Some("/run".into()), None, None, 1, home).expect("resolves");
        assert_eq!(d.config_base, PathBuf::from("/home/ada/.config"));
        assert_eq!(d.state_base, PathBuf::from("/home/ada/.local/state"));
    }

    #[test]
    fn explicit_xdg_config_and_state_override_home() {
        let d = Dirs::resolve(
            Some("/run".into()),
            Some("/cfg".into()),
            Some("/st".into()),
            1,
            || panic!("home must not be consulted when both XDG vars are set"),
        )
        .expect("resolves");
        assert_eq!(d.config_base, PathBuf::from("/cfg"));
        assert_eq!(d.state_base, PathBuf::from("/st"));
    }

    #[test]
    fn an_unresolvable_home_only_fails_when_a_base_actually_needs_it() {
        let no_home = || anyhow::bail!("no home");
        // Both XDG bases present: home is never consulted, so this succeeds.
        Dirs::resolve(
            Some("/run".into()),
            Some("/cfg".into()),
            Some("/st".into()),
            1,
            no_home,
        )
        .expect("home is irrelevant when both XDG bases are set");
        // A missing base forces the lookup, and the failure surfaces.
        let err = Dirs::resolve(Some("/run".into()), None, Some("/st".into()), 1, no_home)
            .expect_err("config base needs home");
        assert!(err.to_string().contains("no home"));
    }

    #[cfg(unix)]
    #[test]
    fn fallback_runtime_parent_must_not_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let redirected = tmp.path().join("redirected");
        std::fs::create_dir(&redirected).expect("create redirect target");
        let fallback = tmp.path().join("kmux-1234");
        symlink(&redirected, &fallback).expect("symlink");

        let error =
            create_runtime_dir(&fallback, true).expect_err("symlinked base must be refused");
        assert!(error.to_string().contains("must not be a symlink"));
        assert!(!redirected.join(KMUX_DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, dirs) = fixture_dirs();
        let dir = dirs.runtime_dir().expect("runtime dir");
        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "the runtime dir holds the auth token and sockets"
        );
    }

    #[test]
    fn runtime_paths_are_distinct_and_live_under_the_runtime_dir() {
        let (_tmp, dirs) = fixture_dirs();
        let root = dirs.runtime_dir().expect("runtime dir");
        let paths = [
            dirs.socket_path().expect("socket"),
            dirs.data_socket_path().expect("data socket"),
            dirs.pid_path().expect("pid"),
            dirs.handoff_socket_path().expect("handoff"),
            dirs.spawn_lock_path().expect("spawn lock"),
            dirs.token_path().expect("token"),
            dirs.boot_log_path().expect("boot log"),
        ];
        for p in &paths {
            assert!(
                p.starts_with(&root),
                "{} escaped the runtime dir",
                p.display()
            );
        }
        let unique: std::collections::BTreeSet<_> = paths.iter().collect();
        assert_eq!(
            unique.len(),
            paths.len(),
            "two runtime paths collide, which would make them share a file: {paths:?}"
        );
    }

    /// Debug and release builds must resolve *different* runtime dirs, or a debug
    /// client could attach to a release daemon (and vice-versa) over a shared
    /// socket. The whole client↔daemon contract relies on both sides computing
    /// the same profile-namespaced path here.
    #[test]
    fn runtime_paths_are_profile_isolated() {
        #[cfg(debug_assertions)]
        assert_eq!(KMUX_DIR_NAME, "kmux-debug");
        #[cfg(not(debug_assertions))]
        assert_eq!(KMUX_DIR_NAME, "kmux");
        assert_ne!("kmux", "kmux-debug", "the two profiles must never collide");

        let (tmp, dirs) = fixture_dirs();
        let namespaced = tmp.path().join("run").join(KMUX_DIR_NAME);
        for p in [
            dirs.socket_path().expect("socket"),
            dirs.data_socket_path().expect("data socket"),
        ] {
            assert!(
                p.starts_with(&namespaced),
                "{} is not namespaced by {KMUX_DIR_NAME}",
                p.display()
            );
        }
    }

    #[test]
    fn state_dir_is_namespaced_by_profile() {
        let (tmp, dirs) = fixture_dirs();
        let dir = dirs.state_dir().expect("state dir");
        assert_eq!(dir, tmp.path().join("state").join(KMUX_DIR_NAME));
        assert!(dir.exists());
    }

    #[test]
    fn state_paths_live_under_the_state_dir() {
        let (_tmp, dirs) = fixture_dirs();
        let root = dirs.state_dir().expect("state dir");
        assert!(
            dirs.client_log_path()
                .expect("client log")
                .ends_with("client.log")
        );
        assert!(
            dirs.daemon_log_path()
                .expect("daemon log")
                .ends_with("daemon.log")
        );
        assert!(dirs.metrics_log_path().expect("metrics").starts_with(&root));
        for p in [
            dirs.daemon_trace_path().expect("daemon trace"),
            dirs.client_trace_path().expect("client trace"),
        ] {
            assert!(
                p.starts_with(&root),
                "{} escaped the state dir",
                p.display()
            );
        }
    }

    #[test]
    fn connection_log_is_named_for_the_instance_and_grouped_in_a_subdir() {
        let (_tmp, dirs) = fixture_dirs();
        let path = dirs
            .connection_log_path("abc123ef")
            .expect("connection log");
        assert!(path.ends_with("abc123ef.log"));
        assert!(path.parent().expect("parent").ends_with("connections"));
        assert!(
            path.parent().expect("parent").exists(),
            "the dir is created eagerly"
        );
    }

    #[test]
    fn session_paths_share_the_sessions_dir_but_are_distinct_files() {
        let (_tmp, dirs) = fixture_dirs();
        let dir = dirs.sessions_dir().expect("sessions dir");
        assert!(dir.exists());
        assert!(dir.ends_with("sessions"));

        let live = dirs.session_state_path().expect("state");
        let closed = dirs.closed_sessions_path().expect("closed");
        assert!(live.ends_with("state.bin"));
        assert!(closed.ends_with("closed.bin"));
        assert_eq!(live.parent(), closed.parent());
        assert_ne!(
            live, closed,
            "the graveyard must not share a file with the live checkpoint"
        );
    }

    #[test]
    fn config_is_shared_across_profiles_unlike_runtime_and_state() {
        let (tmp, dirs) = fixture_dirs();
        // Config deliberately uses the literal `kmux`, never KMUX_DIR_NAME: it
        // records user intent, so a debug build must read the same file a
        // release build wrote.
        assert_eq!(
            dirs.config_dir().expect("config dir"),
            tmp.path().join("config").join("kmux")
        );
    }

    #[test]
    fn config_paths_live_under_the_config_dir() {
        let (_tmp, dirs) = fixture_dirs();
        let root = dirs.config_dir().expect("config dir");
        for p in [
            dirs.known_hosts_path().expect("known hosts"),
            dirs.identity_key_path().expect("identity"),
            dirs.tls_cert_dir().expect("tls dir"),
        ] {
            assert!(
                p.starts_with(&root),
                "{} escaped the config dir",
                p.display()
            );
        }
        assert!(dirs.tls_cert_dir().expect("tls dir").exists());
    }

    #[test]
    fn rooted_creates_nothing_until_an_accessor_is_called() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(tmp.path());
        assert!(
            std::fs::read_dir(tmp.path())
                .expect("read root")
                .next()
                .is_none(),
            "construction must be pure, so a test can build Dirs freely"
        );
        dirs.state_dir().expect("state dir");
        assert!(tmp.path().join("state").exists());
    }
}
