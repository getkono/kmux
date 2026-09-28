//! Shared scaffolding for the daemon end-to-end suites.
//!
//! Each of these suites used to carry its own copy of the same six or seven
//! helpers, and — more consequentially — its own `ENV_LOCK`: a process-global
//! mutex serialising writes to the process-wide `XDG_*` variables so two tests
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
use kmux_client::grid::CellGrid;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, PeerTarget, SequenceNo, ServerMessage, TermSize,
};
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

/// The bound on every e2e wait: a daemon coming up, a reply arriving, a child
/// process writing its pid, a peer link opening.
///
/// One value, because these are all the same kind of wait — something that
/// normally takes milliseconds, bounded only so a *broken* path reports instead
/// of hanging. The bound is therefore generous: it costs time only when the test
/// is already failing, and a tight one fails a correct daemon on a loaded CI
/// runner (the DSR round-trip in `query_response_e2e` did exactly that at 10 s).
/// A shorter bound needs a comment saying why at its call site.
pub const E2E_TIMEOUT: Duration = Duration::from_secs(30);

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
        // start and the only symptom is a full [`E2E_TIMEOUT`] wait.
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

    /// What the daemon in this sandbox has logged so far, for a failure
    /// message: an e2e failure says little without the daemon's side of it.
    #[must_use]
    pub fn daemon_log(&self) -> String {
        self.dirs
            .daemon_log_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
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

/// Poll the async `f` every 50ms until it yields a value or `timeout` elapses.
pub async fn wait_for<T, F: Future<Output = Option<T>>>(
    timeout: Duration,
    mut f: impl FnMut() -> F,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = f().await {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Read a pid written by a child process, waiting for the file to appear and to
/// hold a complete line.
pub async fn read_pid_file(path: &Path, timeout: Duration) -> Option<i32> {
    wait_for(timeout, || async {
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<i32>()
            .ok()
    })
    .await
}

// ─── The daemon under test ───────────────────────────────────────────────────

/// The `kmux-vt-worker` binary, which must already be built next to `kmuxd`.
///
/// `cargo test` builds only this package's binaries, and the worker is another
/// package, so `mise run test` builds it first (the `build-vt-worker` task).
/// The harness only locates it: building from inside a test needed cargo at
/// test time and contended for the target-dir lock with the run it was part of.
#[must_use]
pub fn worker_binary() -> PathBuf {
    let worker = Path::new(env!("CARGO_BIN_EXE_kmuxd")).with_file_name("kmux-vt-worker");
    assert!(
        worker.exists(),
        "kmux-vt-worker not found at {worker:?}. The process-isolation e2e tests \
         need it built first: run `mise run test`, or `cargo build -p kmux-vt-worker` \
         before `cargo test`."
    );
    worker
}

/// A daemon to spawn into a [`Sandbox`].
pub struct Daemon<'a> {
    exe: PathBuf,
    sandbox: &'a Sandbox,
    isolation: Option<&'static str>,
    extra_env: Vec<(String, PathBuf)>,
    config: Option<PathBuf>,
    /// `--port` and `--tcp-port`; ephemeral (`0`) by default.
    ports: (u16, u16),
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
            config: None,
            ports: (0, 0),
        }
    }

    /// Listen on these QUIC and TCP+TLS ports rather than ephemeral ones.
    #[must_use]
    pub fn ports(mut self, quic: u16, tcp: u16) -> Self {
        self.ports = (quic, tcp);
        self
    }

    /// Run with `toml` as its `kmuxd.toml` (`--config`).
    #[must_use]
    pub fn config(mut self, toml: &str) -> Self {
        let path = self.sandbox.path().join("kmuxd.toml");
        std::fs::write(&path, toml).expect("write kmuxd.toml");
        self.config = Some(path);
        self
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
        let worker = worker_binary();
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
        let (quic, tcp) = (self.ports.0.to_string(), self.ports.1.to_string());
        let mut args: Vec<&std::ffi::OsStr> = [
            "--daemon",
            "--bind",
            "127.0.0.1",
            "--port",
            &quic,
            "--tcp-port",
            &tcp,
        ]
        .map(std::ffi::OsStr::new)
        .to_vec();
        if let Some(mode) = self.isolation {
            args.extend(["--session-isolation", mode].map(std::ffi::OsStr::new));
        }
        if let Some(config) = &self.config {
            args.extend([std::ffi::OsStr::new("--config"), config.as_os_str()]);
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
    let deadline = Instant::now() + E2E_TIMEOUT;
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

/// What a running daemon in `sandbox` reports on its control socket.
pub async fn daemon_status(sandbox: &Sandbox) -> kmux_client::daemon::DaemonStatus {
    kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
        .await
        .expect("daemon status")
}

/// The auth token a running daemon published into `sandbox`.
pub async fn daemon_token(sandbox: &Sandbox) -> String {
    daemon_status(sandbox).await.token
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
        match recv_until(&mut rx, E2E_TIMEOUT, |m| {
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
    let created = recv_until(&mut client.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await
    .expect("SessionCreated");
    let ServerMessage::SessionCreated { entry, .. } = created else {
        unreachable!("filtered above")
    };
    let pane_id = format!("{}/0", entry.meta.word_id);
    attach(client, &pane_id, None);
    pane_id
}

/// Attach `client` to `pane_id`, resuming from `last_seqno` if given.
pub fn attach(client: &Client, pane_id: &str, last_seqno: Option<SequenceNo>) {
    let sent = client.tx.send(ClientMessage::Attach {
        pane_id: pane_id.to_string(),
        last_seqno,
        size: SIZE,
    });
    assert!(sent.is_ok(), "send Attach");
}

/// Type `text` into `pane_id` as `client`.
pub fn type_into(client: &Client, pane_id: &str, text: &str) {
    let sent = client.tx.send(ClientMessage::PtyInput {
        pane_id: pane_id.to_string(),
        data: text.as_bytes().to_vec(),
    });
    assert!(sent.is_ok(), "send PtyInput");
}

// ─── A client's view of one pane ─────────────────────────────────────────────

/// One pane's screen as a client rebuilds it from what the daemon sends, and
/// how it got there: which snapshots, resets and diffs it applied.
pub struct Screen {
    pane_id: String,
    grid: CellGrid,
    /// The newest seqno applied: what a client resumes from.
    pub seqno: Option<SequenceNo>,
    /// Every `TerminalUpdate` applied, by seqno.
    pub updates: Vec<SequenceNo>,
    /// How many `TerminalSnapshot`s were applied.
    pub snapshots: usize,
    /// How many `SyncReset`s arrived.
    pub resets: usize,
}

impl Screen {
    #[must_use]
    pub fn new(pane_id: &str) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            grid: CellGrid::new(SIZE.rows.into(), SIZE.cols.into()),
            seqno: None,
            updates: Vec::new(),
            snapshots: 0,
            resets: 0,
        }
    }

    /// The same screen, with its history of how it got there forgotten: a
    /// client that comes back with what it last showed.
    #[must_use]
    pub fn resumed(&self) -> Self {
        let mut resumed = Self::new(&self.pane_id);
        resumed.grid.apply_snapshot(self.grid.to_snapshot());
        resumed.seqno = self.seqno;
        resumed
    }

    /// The grid as one row-major string.
    #[must_use]
    pub fn text(&self) -> String {
        self.grid.to_snapshot().cells.iter().map(|c| c.c).collect()
    }

    /// Apply `msg` if it is for this pane.
    pub fn apply(&mut self, msg: &ServerMessage) {
        match msg {
            ServerMessage::TerminalSnapshot {
                pane_id,
                snapshot,
                seqno,
                ..
            } if *pane_id == self.pane_id => {
                self.grid.apply_snapshot((**snapshot).clone());
                self.snapshots += 1;
                self.seqno = Some(*seqno);
            }
            ServerMessage::TerminalUpdate {
                pane_id,
                diff,
                seqno,
                ..
            } if *pane_id == self.pane_id => {
                self.grid.apply_diff((**diff).clone());
                self.updates.push(*seqno);
                self.seqno = Some(*seqno);
            }
            ServerMessage::CursorUpdate {
                pane_id,
                cursor,
                modes,
                ..
            } if *pane_id == self.pane_id => self.grid.apply_cursor_update(*cursor, *modes),
            ServerMessage::SyncReset { pane_id } if *pane_id == self.pane_id => self.resets += 1,
            _ => {}
        }
    }

    /// Apply what `client` receives until the screen shows `text`. Whether it
    /// does, within [`E2E_TIMEOUT`].
    pub async fn follow_until(&mut self, client: &mut Client, text: &str) -> bool {
        let deadline = Instant::now() + E2E_TIMEOUT;
        while !self.text().contains(text) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, client.rx.recv()).await {
                Ok(Some(msg)) => self.apply(&msg),
                Ok(None) | Err(_) => return false,
            }
        }
        true
    }
}

// ─── Federation ──────────────────────────────────────────────────────────────

/// Two sandboxed daemons for the federation suite: a *remote* reachable over
/// TCP+TLS and a *local* hub the mock GUI talks to over its UDS.
///
/// Field order is drop order: `cleanup` SIGKILLs both daemons (and whatever
/// else a test tracked) before the sandboxes delete the trees they run in.
pub struct Federation {
    pub cleanup: Cleanup,
    pub remote: Sandbox,
    pub local: Sandbox,
    pub remote_pid: u32,
    pub local_pid: u32,
    /// The remote's auth token — what `OpenPeer` presents upstream.
    pub remote_token: String,
    /// The remote's ephemeral TCP+TLS port, the federation endpoint.
    pub remote_tcp: u16,
    /// The hub's auth token — what a GUI presents to the local daemon.
    pub local_token: String,
}

impl Federation {
    /// Start both daemons and read the tokens and port a test needs.
    pub async fn spawn_pair() -> Self {
        let cleanup = Cleanup::default();
        let remote = Sandbox::new();
        let local = Sandbox::new();

        let remote_pid = Daemon::new(&remote).spawn(None).await;
        cleanup.track(remote_pid as i32);
        let remote_status = daemon_status(&remote).await;
        assert_ne!(
            remote_status.tcp_port, 0,
            "remote daemon must expose an ephemeral TCP+TLS port for federation"
        );

        let local_pid = Daemon::new(&local).spawn(None).await;
        cleanup.track(local_pid as i32);
        let local_token = daemon_token(&local).await;

        Self {
            cleanup,
            remote,
            local,
            remote_pid,
            local_pid,
            remote_token: remote_status.token,
            remote_tcp: remote_status.tcp_port,
            local_token,
        }
    }

    /// The remote as a direct peer target, authenticating with `token`.
    #[must_use]
    pub fn remote_target(&self, token: &str) -> PeerTarget {
        PeerTarget::Direct {
            host: "127.0.0.1".into(),
            port: self.remote_tcp,
            token: token.to_string(),
            accept_invalid_certs: true,
        }
    }

    /// A GUI connected to the hub.
    pub async fn connect_gui(&self) -> Client {
        connect_client(&self.local, &self.local_token).await
    }

    /// Connect a GUI to the hub and federate the hub to the remote through it.
    /// Returns the GUI and the peer id the hub assigned.
    pub async fn open_peer(&self) -> (Client, String) {
        let mut gui = self.connect_gui().await;
        let sent = gui.tx.send(ClientMessage::OpenPeer {
            request_id: 1,
            target: self.remote_target(&self.remote_token),
        });
        assert!(sent.is_ok(), "send OpenPeer");
        let reply = recv_until(&mut gui.rx, E2E_TIMEOUT, |m| {
            matches!(
                m,
                ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
            )
        })
        .await;
        let peer = match &reply {
            Some(ServerMessage::PeerOpened { peer, .. }) => Some(peer.clone()),
            _ => None,
        };
        assert!(peer.is_some(), "federation must open, got {reply:?}");
        (gui, peer.unwrap_or_default())
    }

    /// Stop both daemons (a remote a test already killed is simply not there).
    pub async fn shutdown(self) {
        let _ = kmux_client::daemon::stop_daemon_at(&self.local.socket_path()).await;
        let _ = kmux_client::daemon::stop_daemon_at(&self.remote.socket_path()).await;
    }
}
