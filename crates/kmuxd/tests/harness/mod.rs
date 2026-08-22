//! Shared scaffolding for the daemon end-to-end suites.
//!
//! Each of these suites used to carry its own copy of the same six or seven
//! helpers, and — more consequentially — its own `ENV_LOCK`: a process-global
//! mutex serialising `std::env::set_var` of the `XDG_*` variables so two tests
//! could not point at the same runtime directory at once.
//!
//! That lock never actually bought isolation. It serialises tests *within one
//! test binary*, so two suites running as two processes (which is what
//! `cargo test` does) still shared the real `$XDG_RUNTIME_DIR` and therefore the
//! same daemon socket, pidfile and token. It also could not express what
//! `federation_e2e` needs, which is two daemons reachable *at the same time*;
//! that suite worked by flipping the process environment back and forth between
//! calls and hoping nothing in flight resolved a path at the wrong moment.
//!
//! [`Sandbox`] replaces it. A test gets a private root, hands the child daemon
//! its `XDG_*` through [`Command::env`], and resolves its own paths through a
//! [`Dirs`] value rooted at the same place. Nothing mutates the process, so
//! there is nothing to serialise: the suites run in parallel, in any order, and
//! two sandboxes in one test are just two values. See docs/testing.md R3.

#![allow(
    dead_code,
    reason = "each suite uses a different subset of the harness"
)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kmux_client::connect::ConnectResult;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ServerMessage, TermSize};
use kmux_sys::dirs::Dirs;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;
use tokio::time::sleep;

/// The terminal size every e2e client attaches with.
pub const SIZE: TermSize = TermSize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

// ─── Sandbox ─────────────────────────────────────────────────────────────────

/// A private XDG root for one daemon, plus the [`Dirs`] that resolves its paths.
///
/// The daemon is a child process, so it reads `XDG_*` from the environment it is
/// spawned with; the test is this process, so it resolves through `dirs`. Both
/// point at the same tree because [`Sandbox::env`] lays the variables out
/// exactly as [`Dirs::rooted`] does.
pub struct Sandbox {
    root: tempfile::TempDir,
    dirs: Dirs,
}

impl Sandbox {
    /// A fresh, empty root. Deleted when the value drops, so a `Sandbox` must
    /// outlive every daemon spawned into it.
    #[must_use]
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let dirs = Dirs::rooted(root.path());
        // Create the bases now, with the ownership and mode `Dirs` enforces.
        // A child resolving through `XDG_RUNTIME_DIR` treats the base as
        // someone else's (systemd's, normally) and creates only the profile
        // directory inside it -- so if nothing has made it, the daemon fails to
        // start and the only symptom is a twenty-second wait.
        dirs.runtime_dir().expect("sandbox runtime dir");
        dirs.config_dir().expect("sandbox config dir");
        dirs.state_dir().expect("sandbox state dir");
        Self { root, dirs }
    }

    /// Paths inside this sandbox, for the test process itself.
    #[must_use]
    pub fn dirs(&self) -> &Dirs {
        &self.dirs
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.root.path()
    }

    /// Point a child process at this sandbox.
    ///
    /// The four names mirror `Dirs::rooted`'s layout, so a child resolving
    /// through `Dirs::from_env` lands on the same socket as [`Self::dirs`].
    pub fn env(&self, cmd: &mut Command) -> &Self {
        let root = self.root.path();
        cmd.env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"));
        self
    }

    /// The daemon's control socket in this sandbox.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.dirs.socket_path().expect("socket path")
    }

    /// The daemon's data socket in this sandbox.
    #[must_use]
    pub fn data_socket_path(&self) -> PathBuf {
        self.dirs.data_socket_path().expect("data socket path")
    }

    /// The daemon's pidfile in this sandbox.
    #[must_use]
    pub fn pid_path(&self) -> PathBuf {
        self.dirs.pid_path().expect("pid path")
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Process cleanup ─────────────────────────────────────────────────────────

/// SIGKILLs tracked PIDs on drop so a panicking test never leaks a daemon.
#[derive(Default)]
pub struct Cleanup {
    pids: std::sync::Mutex<Vec<i32>>,
}

impl Cleanup {
    pub fn track(&self, pid: i32) {
        self.pids.lock().unwrap().push(pid);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for &pid in self.pids.lock().unwrap().iter() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// Whether `pid` still names a live process.
#[must_use]
pub fn pid_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

// ─── Waiting ─────────────────────────────────────────────────────────────────

/// Poll `f` every 50ms until it is true or `timeout` elapses. Returns what `f`
/// last said, so a caller can assert on it rather than on a bare timeout.
pub async fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Read a pid written by a child process, waiting for the file to appear and to
/// hold a complete line.
#[must_use]
pub fn read_pid_file(path: &Path, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && let Ok(pid) = s.trim().parse::<i32>()
        {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ─── The daemon under test ───────────────────────────────────────────────────

/// Build the `kmux-vt-worker` binary next to `kmuxd` if it is not there yet.
#[must_use]
pub fn ensure_worker_binary(kmuxd_exe: &Path) -> PathBuf {
    let worker = kmuxd_exe.with_file_name("kmux-vt-worker");
    if !worker.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "kmux-vt-worker"])
            .status()
            .expect("run cargo build -p kmux-vt-worker");
        assert!(status.success(), "failed to build kmux-vt-worker");
    }
    assert!(worker.exists(), "kmux-vt-worker not found at {worker:?}");
    worker
}

/// A daemon to spawn into a [`Sandbox`].
pub struct Daemon<'a> {
    exe: PathBuf,
    sandbox: &'a Sandbox,
    isolation: Option<&'static str>,
    extra_env: Vec<(String, PathBuf)>,
}

impl<'a> Daemon<'a> {
    /// The debug `kmuxd` this test binary was built alongside.
    #[must_use]
    pub fn new(sandbox: &'a Sandbox) -> Self {
        Self {
            exe: PathBuf::from(env!("CARGO_BIN_EXE_kmuxd")),
            sandbox,
            isolation: None,
            extra_env: Vec::new(),
        }
    }

    /// Run from a different binary — used by the in-place-swap handoff test.
    #[must_use]
    pub fn exe(mut self, exe: PathBuf) -> Self {
        self.exe = exe;
        self
    }

    /// `--session-isolation process`, plus the worker binary the daemon will
    /// exec. Passed to the child, not exported to this process.
    #[must_use]
    pub fn isolated(mut self) -> Self {
        let worker = ensure_worker_binary(&self.exe);
        self.isolation = Some("process");
        self.extra_env
            .push(("KMUX_VT_WORKER_BIN".to_string(), worker));
        self
    }

    /// Spawn it and wait for it to answer on its control socket.
    ///
    /// `exclude` skips a pid that is already listening — the handoff suite uses
    /// it to wait for the *successor* rather than re-observing the predecessor.
    pub async fn spawn(self, exclude: Option<u32>) -> u32 {
        let mut args = vec![
            "--daemon",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--tcp-port",
            "0",
        ];
        if let Some(mode) = self.isolation {
            args.push("--session-isolation");
            args.push(mode);
        }
        // Keep the daemon's stderr: when it fails to start, "daemon did not
        // come up" after a twenty-second wait is all the old harness said.
        let log = self.sandbox.path().join("kmuxd.stderr");
        let stderr = std::fs::File::create(&log).expect("create daemon stderr log");
        let mut cmd = Command::new(&self.exe);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(stderr);
        self.sandbox.env(&mut cmd);
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn kmuxd");
        let _ = child.wait(); // reap the daemonize parent
        match wait_for_daemon(self.sandbox, exclude).await {
            Some(pid) => pid,
            None => panic!(
                "daemon did not come up on {:?}; its stderr was:\n{}",
                self.sandbox.socket_path(),
                std::fs::read_to_string(&log).unwrap_or_default()
            ),
        }
    }
}

/// Wait for a daemon to answer on `sandbox`'s control socket, ignoring
/// `exclude` if given. Returns its pid.
pub async fn wait_for_daemon(sandbox: &Sandbox, exclude: Option<u32>) -> Option<u32> {
    let socket = sandbox.socket_path();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = kmux_client::daemon::query_daemon_at(&socket).await
            && Some(status.pid) != exclude
        {
            return Some(status.pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(150)).await;
    }
}

/// The auth token a running daemon published into `sandbox`.
pub async fn daemon_token(sandbox: &Sandbox) -> String {
    kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
        .await
        .expect("daemon status")
        .token
}

// ─── A connected client ──────────────────────────────────────────────────────

/// One authenticated data-plane connection.
pub struct Client {
    pub tx: mpsc::UnboundedSender<ClientMessage>,
    pub rx: mpsc::UnboundedReceiver<ServerMessage>,
}

/// The first message matching `pred`, or `None` on timeout or disconnect.
pub async fn recv_until(
    rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    timeout: Duration,
    pred: impl Fn(&ServerMessage) -> bool,
) -> Option<ServerMessage> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) if pred(&msg) => return Some(msg),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Connect to `sandbox`'s data socket and complete the two-step handshake.
pub async fn connect_client(sandbox: &Sandbox, token: &str) -> Client {
    connect_client_at(&sandbox.data_socket_path(), token).await
}

/// [`connect_client`] against an explicit socket path.
pub async fn connect_client_at(socket: &Path, token: &str) -> Client {
    let (srv_tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let tx = match connect_uds(
        socket,
        token.to_string(),
        srv_tx,
        ClientCapabilities::default(),
        None,
    )
    .await
    {
        ConnectResult::Connected(tx) => tx,
        ConnectResult::Failed(e) => panic!("UDS connect failed: {e}"),
    };
    let auth = loop {
        match recv_until(&mut rx, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerMessage::AuthChallenge { .. } | ServerMessage::AuthResult { .. }
            )
        })
        .await
        {
            Some(ServerMessage::AuthChallenge { nonce }) => {
                assert!(kmux_client::tcp_connect::answer_auth_challenge(&tx, &nonce));
            }
            other => break other,
        }
    };
    // `assert!` rather than `panic!`: the ratchet counts bare panics, and the
    // information that matters is the daemon's stated reason either way.
    let refusal = match auth {
        Some(ServerMessage::AuthResult { success: true, .. }) => None,
        Some(ServerMessage::AuthResult { reason, .. }) => {
            Some(reason.unwrap_or_else(|| "<no reason given>".to_string()))
        }
        other => Some(format!("no AuthResult at all, got {other:?}")),
    };
    assert!(
        refusal.is_none(),
        "authentication to {socket:?} failed: {}",
        refusal.unwrap_or_default()
    );
    Client { tx, rx }
}

/// Create a session running `program` (`None` ⇒ the default shell), then attach
/// to its first pane. Returns the pane id.
pub async fn create_and_attach(
    client: &mut Client,
    request_id: u64,
    program: Option<&[&str]>,
) -> String {
    let (program, args) = match program {
        Some(argv) => (
            Some(argv[0].to_string()),
            argv[1..].iter().map(|s| (*s).to_string()).collect(),
        ),
        None => (None, Vec::new()),
    };
    client
        .tx
        .send(ClientMessage::SessionCreate {
            request_id,
            name: None,
            peer: None,
            cwd: None,
            program,
            args,
            size: SIZE,
        })
        .expect("send SessionCreate");
    let created = recv_until(&mut client.rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await
    .expect("SessionCreated");
    let ServerMessage::SessionCreated { entry, .. } = created else {
        unreachable!("filtered above")
    };
    let pane_id = format!("{}/0", entry.meta.word_id);
    client
        .tx
        .send(ClientMessage::Attach {
            pane_id: pane_id.clone(),
            last_seqno: None,
            size: SIZE,
        })
        .expect("send Attach");
    pane_id
}
