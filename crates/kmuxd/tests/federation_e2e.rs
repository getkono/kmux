//! Cross-process integration test for daemon federation (issue #121).
//!
//! Spawns two real `kmuxd` processes (via `CARGO_BIN_EXE_kmuxd`) at isolated
//! `XDG_*` dirs — a *remote* daemon hosting a real PTY session and a *local*
//! daemon — then drives a mock GUI (a raw UDS client) against the **local**
//! daemon. The GUI issues `OpenPeer { Direct }` to federate the local daemon to
//! the remote over TCP+TLS, lists sessions through the local daemon, and attaches
//! to the remote session *through* the local daemon.
//!
//! It asserts both directions end-to-end, with pane-ID translation in between:
//!   * **output** — the remote session's startup marker arrives at the GUI in a
//!     `TerminalSnapshot` addressed by the **local** pane ID, and
//!   * **input** — a command typed into the GUI runs on the **remote** PTY
//!     (it `touch`es a file the test then observes).
//!
//! This exercises `PeerManager::open_peer` (connect + auth + session list +
//! local registration), the dispatch branching, and the upstream feed loop.
//! Gated on the `federation` feature (default-on for kmuxd).

#![cfg(all(unix, feature = "federation"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kmux_client::connect::ConnectResult;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, PeerTarget, ServerMessage, SessionEventMsg, TermSize,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;

/// Serializes the test against any other that mutates process-global `XDG_*`
/// env vars (the dirs helpers and the spawned daemons read them to resolve
/// sockets). Held for the whole test.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const ATTACH_SIZE: TermSize = TermSize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Point the current process's XDG dirs at an isolated temp dir. Spawned daemons
/// inherit these; the kmux-client control helpers read them to resolve sockets.
/// Caller must hold [`ENV_LOCK`].
fn set_xdg(dir: &Path) {
    for key in [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        // SAFETY: guarded by ENV_LOCK; no other test mutates env concurrently.
        unsafe { std::env::set_var(key, dir) };
    }
}

/// SIGKILLs every tracked PID on drop so a panicking test never leaks a daemon
/// or an orphaned shell.
#[derive(Default)]
struct Cleanup {
    pids: std::sync::Mutex<Vec<i32>>,
}

impl Cleanup {
    fn track(&self, pid: i32) {
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

/// Spawn `exe` as a background daemon (isolated dirs already set) and return the
/// PID the control socket reports once it is up. Binds loopback with an ephemeral
/// TCP+TLS port so a second daemon can reach it for federation.
async fn spawn_daemon(exe: &Path) -> u32 {
    let mut child = Command::new(exe)
        .args([
            "--daemon",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--tcp-port",
            "0",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn kmuxd");
    // The daemonize parent forks the real daemon and exits; reap it so it does
    // not linger as a zombie. The daemon is tracked via the control socket.
    let _ = child.wait();
    wait_for_daemon()
        .await
        .expect("daemon did not come up within the deadline")
}

/// Poll the control socket (resolved from the *current* XDG dirs) until a live
/// daemon is reported. Returns its PID, or `None` on timeout.
async fn wait_for_daemon() -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = kmux_client::daemon::query_daemon().await {
            return Some(status.pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Await `f` becoming true (sync predicate), polling until `timeout`.
async fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Receive from `rx` until a message matches `pred` or `timeout` elapses.
async fn recv_until(
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

/// Connect a mock client to the data UDS resolved from the current XDG dirs and
/// authenticate. Returns the upstream sink and the server-message receiver.
async fn connect_authenticated(
    token: &str,
) -> (
    mpsc::UnboundedSender<ClientMessage>,
    mpsc::UnboundedReceiver<ServerMessage>,
) {
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let data_sock = kmux_protocol::dirs::data_socket_path().expect("data socket path");
    let client_tx = match connect_uds(
        &data_sock,
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
    // Answer the identity challenge, then await the successful result (issue #146).
    let auth = loop {
        match recv_until(&mut srv_rx, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerMessage::AuthChallenge { .. } | ServerMessage::AuthResult { .. }
            )
        })
        .await
        {
            Some(ServerMessage::AuthChallenge { nonce }) => {
                assert!(
                    kmux_client::tcp_connect::answer_auth_challenge(&client_tx, &nonce),
                    "answering the identity challenge must succeed"
                );
            }
            other => break other,
        }
    };
    assert!(
        matches!(auth, Some(ServerMessage::AuthResult { success: true, .. })),
        "expected a successful AuthResult"
    );
    (client_tx, srv_rx)
}

/// On the daemon reachable at the current XDG dirs, create a session whose pane
/// prints `marker` then `exec`s an interactive shell (so it both shows the marker
/// in its grid and executes typed input). Records the shell's PID to `pidfile`.
/// Returns `(remote_word_id, shell_pid)`.
async fn create_remote_session(
    token: &str,
    cwd: &Path,
    marker: &str,
    pidfile: &Path,
) -> (String, i32) {
    let (client_tx, mut srv_rx) = connect_authenticated(token).await;

    let script = format!("echo $$ > {}; echo {marker}; exec sh", pidfile.display());
    client_tx
        .send(ClientMessage::SessionCreate {
            request_id: 1,
            name: Some("fed-src".into()),
            peer: None,
            cwd: Some(cwd.display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script],
            size: ATTACH_SIZE,
        })
        .expect("send SessionCreate");

    let created = recv_until(&mut srv_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await
    .expect("expected a SessionCreated ack");
    let word_id = match created {
        ServerMessage::SessionCreated { entry, .. } => entry.meta.word_id,
        _ => unreachable!(),
    };

    let pid = read_pid_file(pidfile, Duration::from_secs(5)).expect("shell wrote its PID");
    // Drop the client; the daemon keeps the session alive without it.
    drop(client_tx);
    (word_id, pid)
}

/// Block until `path` holds a parseable PID, or time out.
fn read_pid_file(path: &Path, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Flatten a grid snapshot's cells into a string (row-major) for marker scanning.
fn snapshot_text(snapshot: &kmux_protocol::messages::GridSnapshot) -> String {
    snapshot.cells.iter().map(|c| c.c).collect()
}

/// Ask the local daemon for its session list and return the federated session's
/// (peer-decorated) first pane ID.
async fn federated_pane(
    tx: &mpsc::UnboundedSender<ClientMessage>,
    rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
) -> String {
    tx.send(ClientMessage::SessionList { request_id: 100 })
        .expect("send SessionList");
    let list = recv_until(rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionListResult { .. })
    })
    .await
    .expect("expected a SessionListResult");
    let ServerMessage::SessionListResult { sessions, .. } = list else {
        unreachable!("recv_until only yields a SessionListResult here")
    };
    sessions
        .iter()
        .find(|e| e.meta.name.contains('@'))
        .expect("the federated session should appear in the local list")
        .panes[0]
        .pane_id
        .clone()
}

/// The headline #121 path: one GUI attaches to a remote session *through* the
/// local daemon over a single federated link, and both input and output flow.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn gui_attaches_to_remote_session_through_local_daemon() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // ── Remote daemon: host a real session with a known startup marker. ──
    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    assert!(
        remote_tcp != 0,
        "remote daemon must expose an ephemeral TCP+TLS port for federation"
    );

    const MARKER: &str = "FEDMARKER_OUTPUT";
    let pidfile = remote_dir.path().join("shell.pid");
    let (remote_word, shell_pid) =
        create_remote_session(&remote_token, remote_dir.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    // ── Local daemon: the per-user hub the GUI actually talks to. ──
    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    // ── Mock GUI → local daemon (UDS). ──
    let (gui_tx, mut gui_rx) = connect_authenticated(&local_token).await;

    // 1. Federate the local daemon to the remote over a direct TCP+TLS endpoint.
    gui_tx
        .send(ClientMessage::OpenPeer {
            request_id: 10,
            target: PeerTarget::Direct {
                host: "127.0.0.1".into(),
                port: remote_tcp,
                token: remote_token.clone(),
                accept_invalid_certs: true,
            },
        })
        .expect("send OpenPeer");
    let opened = recv_until(&mut gui_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    })
    .await
    .expect("expected a PeerOpened/PeerError reply");
    match opened {
        ServerMessage::PeerOpened { peer, .. } => {
            assert_eq!(peer, format!("127.0.0.1:{remote_tcp}"));
        }
        ServerMessage::PeerError { reason, .. } => panic!("federation failed: {reason}"),
        _ => unreachable!(),
    }

    // 2. The remote's session must appear in the local daemon's session list,
    //    under a *local* word ID (not the remote's) and a peer-decorated name.
    gui_tx
        .send(ClientMessage::SessionList { request_id: 11 })
        .expect("send SessionList");
    let list = recv_until(&mut gui_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionListResult { .. })
    })
    .await
    .expect("expected a SessionListResult");
    let ServerMessage::SessionListResult { sessions, .. } = list else {
        unreachable!("recv_until only yields a SessionListResult here")
    };
    let entry = sessions
        .iter()
        .find(|e| e.meta.name.contains('@'))
        .expect("the federated session should appear in the local list");
    assert_ne!(
        entry.meta.word_id, remote_word,
        "federated sessions must get a freshly-assigned local word ID"
    );
    let local_pane = entry.panes[0].pane_id.clone();
    assert!(
        local_pane.starts_with(&entry.meta.word_id),
        "pane ID must be namespaced under the local word"
    );

    // 3. Attach to the federated pane and assert the remote's startup output
    //    arrives — addressed by the *local* pane ID (the feed loop translated it).
    gui_tx
        .send(ClientMessage::Attach {
            pane_id: local_pane.clone(),
            last_seqno: None,
            size: ATTACH_SIZE,
        })
        .expect("send Attach");
    let want_pane = local_pane.clone();
    let snapshot = recv_until(&mut gui_rx, Duration::from_secs(15), move |m| {
        matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == want_pane)
    })
    .await
    .expect("expected a TerminalSnapshot for the federated pane");
    match snapshot {
        ServerMessage::TerminalSnapshot { snapshot, .. } => {
            assert!(
                snapshot_text(&snapshot).contains(MARKER),
                "the federated snapshot must carry the remote session's output"
            );
        }
        _ => unreachable!(),
    }

    // 4. Input typed into the GUI must run on the *remote* PTY. Drive a `touch`
    //    and observe the file appear on the remote daemon's host.
    let input_marker = remote_dir.path().join("fed_input_marker");
    assert!(!input_marker.exists());
    let cmd = format!("touch {}\n", input_marker.display());
    gui_tx
        .send(ClientMessage::PtyInput {
            pane_id: local_pane.clone(),
            data: cmd.into_bytes(),
        })
        .expect("send PtyInput");
    assert!(
        poll_until(Duration::from_secs(15), || input_marker.exists()).await,
        "GUI input must reach the remote PTY and create the marker file"
    );

    // 5. Session-scoped events propagate: setting the window title on the remote
    //    pane (OSC 2) must reach the GUI as a `PaneTitleChanged` addressed by the
    //    LOCAL pane ID (the feed loop translated the event's pane ID).
    let title_cmd = "printf '\\033]2;FEDTITLE_XYZ\\007'\n";
    gui_tx
        .send(ClientMessage::PtyInput {
            pane_id: local_pane.clone(),
            data: title_cmd.as_bytes().to_vec(),
        })
        .expect("send title-setting input");
    let want_title_pane = local_pane.clone();
    let title_evt = recv_until(&mut gui_rx, Duration::from_secs(15), move |m| {
        matches!(m, ServerMessage::Event {
            event: SessionEventMsg::PaneTitleChanged { pane_id, title },
        } if *pane_id == want_title_pane && title.contains("FEDTITLE_XYZ"))
    })
    .await;
    assert!(
        title_evt.is_some(),
        "a title change on the remote pane must reach the GUI as PaneTitleChanged for the local pane"
    );

    // ── Teardown. ──
    drop(gui_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
    set_xdg(remote_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}

/// Creating a session on a federated peer (issue #121 launcher): the GUI sends
/// `SessionCreate { peer: Some(..) }` to the hub, which forwards it upstream,
/// registers the result under a local word, and replies `SessionCreated` with the
/// session attributed to its peer. The new session must run on the *remote* host.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn gui_creates_a_session_on_a_federated_peer() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // ── Remote daemon: starts with no sessions; the hub will create one on it. ──
    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    assert!(remote_tcp != 0, "remote daemon must expose a TCP+TLS port");

    // ── Local hub daemon: what the GUI talks to. ──
    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    let (gui_tx, mut gui_rx) = connect_authenticated(&local_token).await;

    // 1. Federate the hub to the remote.
    gui_tx
        .send(ClientMessage::OpenPeer {
            request_id: 10,
            target: PeerTarget::Direct {
                host: "127.0.0.1".into(),
                port: remote_tcp,
                token: remote_token.clone(),
                accept_invalid_certs: true,
            },
        })
        .expect("send OpenPeer");
    let peer_id = match recv_until(&mut gui_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    })
    .await
    .expect("expected a PeerOpened/PeerError reply")
    {
        ServerMessage::PeerOpened { peer, .. } => peer,
        ServerMessage::PeerError { reason, .. } => panic!("federation failed: {reason}"),
        _ => unreachable!(),
    };

    // 2. Create a new session ON the peer. The shell records its PID so we can
    //    prove a live *remote* PTY was spawned (and clean it up).
    let pidfile = remote_dir.path().join("created.pid");
    let script = format!("echo $$ > {}; exec sleep 600", pidfile.display());
    gui_tx
        .send(ClientMessage::SessionCreate {
            request_id: 20,
            name: Some("made-on-remote".into()),
            cwd: Some(remote_dir.path().display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script],
            size: ATTACH_SIZE,
            peer: Some(peer_id.clone()),
        })
        .expect("send SessionCreate on peer");
    let entry = match recv_until(&mut gui_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::SessionCreated { .. } | ServerMessage::Error { .. }
        )
    })
    .await
    .expect("expected a SessionCreated/Error reply")
    {
        ServerMessage::SessionCreated { entry, .. } => entry,
        ServerMessage::Error { message, .. } => panic!("remote create failed: {message}"),
        _ => unreachable!(),
    };

    // The reply is attributed to the peer and addressed by a fresh local word.
    assert_eq!(
        entry.peer.as_deref(),
        Some(peer_id.as_str()),
        "a session created on a peer must be attributed to it"
    );
    assert!(entry.meta.name.contains("made-on-remote"));
    let local_pane = entry.panes[0].pane_id.clone();
    assert!(
        local_pane.starts_with(&entry.meta.word_id),
        "pane ID must be namespaced under the local word"
    );

    // 3. The shell really ran on the remote host: its PID file appears there.
    let shell_pid = read_pid_file(&pidfile, Duration::from_secs(15))
        .expect("peer-created shell must write PID");
    cleanup.track(shell_pid);

    // 4. The new session appears in the hub's merged list, attributed to its peer.
    gui_tx
        .send(ClientMessage::SessionList { request_id: 21 })
        .expect("send SessionList");
    let ServerMessage::SessionListResult { sessions, .. } =
        recv_until(&mut gui_rx, Duration::from_secs(5), |m| {
            matches!(m, ServerMessage::SessionListResult { .. })
        })
        .await
        .expect("expected a SessionListResult")
    else {
        unreachable!("recv_until only yields a SessionListResult here")
    };
    assert!(
        sessions
            .iter()
            .any(|e| e.meta.word_id == entry.meta.word_id
                && e.peer.as_deref() == Some(peer_id.as_str())),
        "the peer-created session must appear in the merged list, attributed to its peer"
    );

    // ── Teardown. ──
    drop(gui_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
    set_xdg(remote_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}

/// PR4 reconciliation: two local GUIs share **one** proxied pane over a single
/// federated link. A smaller second viewer shrinks the shared pane (smallest-wins),
/// and the late viewer is served the live mirror's content.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn two_guis_share_one_proxied_pane_with_smallest_wins() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // Remote daemon hosting a marked session, then the local hub.
    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;

    const MARKER: &str = "SHARED_PANE_MARKER";
    let pidfile = remote_dir.path().join("shell.pid");
    let (_remote_word, shell_pid) =
        create_remote_session(&remote_token, remote_dir.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    let big = TermSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    let small = TermSize {
        rows: 10,
        cols: 40,
        pixel_width: 0,
        pixel_height: 0,
    };

    // GUI-1 federates and attaches at the LARGE size.
    let (gui1_tx, mut gui1_rx) = connect_authenticated(&local_token).await;
    gui1_tx
        .send(ClientMessage::OpenPeer {
            request_id: 1,
            target: PeerTarget::Direct {
                host: "127.0.0.1".into(),
                port: remote_tcp,
                token: remote_token.clone(),
                accept_invalid_certs: true,
            },
        })
        .expect("send OpenPeer");
    let opened = recv_until(&mut gui1_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    })
    .await
    .expect("expected a peer reply");
    assert!(
        matches!(opened, ServerMessage::PeerOpened { .. }),
        "federation must open: {opened:?}"
    );

    let local_pane = federated_pane(&gui1_tx, &mut gui1_rx).await;
    gui1_tx
        .send(ClientMessage::Attach {
            pane_id: local_pane.clone(),
            last_seqno: None,
            size: big,
        })
        .expect("gui1 Attach");
    // Baseline: GUI-1 is the sole viewer, so the shared pane is at its large size.
    let want = local_pane.clone();
    let snap1 = recv_until(
        &mut gui1_rx,
        Duration::from_secs(15),
        move |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == want),
    )
    .await
    .expect("gui1 snapshot");
    assert!(
        matches!(&snap1, ServerMessage::TerminalSnapshot { snapshot, .. } if snapshot.rows == 24),
        "the sole viewer should see the pane at its own size"
    );

    // GUI-2 (a second connection to the SAME local daemon) sees the session via the
    // already-open peer — one shared upstream link — and attaches at the SMALL size.
    let (gui2_tx, mut gui2_rx) = connect_authenticated(&local_token).await;
    let local_pane2 = federated_pane(&gui2_tx, &mut gui2_rx).await;
    assert_eq!(
        local_pane2, local_pane,
        "both GUIs must see the same federated pane through one peer"
    );
    gui2_tx
        .send(ClientMessage::Attach {
            pane_id: local_pane.clone(),
            last_seqno: None,
            size: small,
        })
        .expect("gui2 Attach");

    // Late-attach minting: GUI-2 is served the live mirror's content (the marker).
    let want2 = local_pane.clone();
    let snap2 = recv_until(
        &mut gui2_rx,
        Duration::from_secs(15),
        move |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == want2),
    )
    .await
    .expect("gui2 snapshot");
    assert!(
        matches!(&snap2, ServerMessage::TerminalSnapshot { snapshot, .. } if snapshot_text(snapshot).contains(MARKER)),
        "the late viewer must be served the shared pane's content"
    );

    // Smallest-wins: GUI-2 (10×40) joining shrinks the shared pane, so GUI-1 (24×80)
    // receives a snapshot resized DOWN to 10 rows. With last-writer-wins this would
    // never arrive (GUI-2's larger... smaller size would be ignored or GUI-1 kept big).
    let want3 = local_pane.clone();
    let shrunk = recv_until(&mut gui1_rx, Duration::from_secs(15), move |m| {
        matches!(m, ServerMessage::TerminalSnapshot { pane_id, snapshot, .. }
            if *pane_id == want3 && snapshot.rows == 10)
    })
    .await;
    assert!(
        shrunk.is_some(),
        "a smaller second viewer must shrink the shared pane to the min size (smallest-wins)"
    );

    // ── Teardown. ──
    drop(gui1_tx);
    drop(gui2_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
    set_xdg(remote_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}

/// PR6 hardening: when the remote daemon dies, the failure is isolated. The GUI's
/// federated session is cleanly closed (not left hanging), and the local daemon
/// keeps serving — proxied panes live apart from locally-hosted ones.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn remote_daemon_death_is_isolated_from_local_daemon() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // Remote daemon + a session, then the local hub.
    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    let pidfile = remote_dir.path().join("shell.pid");
    let (_remote_word, shell_pid) =
        create_remote_session(&remote_token, remote_dir.path(), "ISO_MARKER", &pidfile).await;
    cleanup.track(shell_pid);

    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    // GUI federates and attaches to the remote session through the local daemon.
    let (gui_tx, mut gui_rx) = connect_authenticated(&local_token).await;
    gui_tx
        .send(ClientMessage::OpenPeer {
            request_id: 1,
            target: PeerTarget::Direct {
                host: "127.0.0.1".into(),
                port: remote_tcp,
                token: remote_token.clone(),
                accept_invalid_certs: true,
            },
        })
        .expect("send OpenPeer");
    let opened = recv_until(&mut gui_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    })
    .await
    .expect("expected a peer reply");
    assert!(
        matches!(opened, ServerMessage::PeerOpened { .. }),
        "federation must open: {opened:?}"
    );
    let local_pane = federated_pane(&gui_tx, &mut gui_rx).await;
    let local_word = local_pane.split('/').next().unwrap().to_string();
    gui_tx
        .send(ClientMessage::Attach {
            pane_id: local_pane.clone(),
            last_seqno: None,
            size: ATTACH_SIZE,
        })
        .expect("gui Attach");
    let want = local_pane.clone();
    assert!(
        recv_until(&mut gui_rx, Duration::from_secs(15), move |m| {
            matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == want)
        })
        .await
        .is_some(),
        "must attach to the federated pane before the remote dies"
    );

    // Kill the remote daemon hard — its TCP link drops under the local daemon.
    let _ = kill(Pid::from_raw(remote_pid as i32), Signal::SIGKILL);

    // Isolation #1: the GUI's federated session is closed cleanly, not hung.
    let want_word = local_word.clone();
    let closed = recv_until(&mut gui_rx, Duration::from_secs(15), move |m| {
        matches!(m, ServerMessage::Event {
            event: SessionEventMsg::SessionClosed { word_id },
        } if *word_id == want_word)
    })
    .await;
    assert!(
        closed.is_some(),
        "a dead peer must surface as SessionClosed for its federated session"
    );

    // Isolation #2: the local daemon is unaffected — its connection to the GUI is
    // live and it still serves new sessions.
    gui_tx
        .send(ClientMessage::SessionCreate {
            request_id: 2,
            name: Some("local-after-death".into()),
            peer: None,
            cwd: Some(local_dir.path().display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exec sleep 600".into()],
            size: ATTACH_SIZE,
        })
        .expect("send local SessionCreate");
    let created = recv_until(&mut gui_rx, Duration::from_secs(10), |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await;
    assert!(
        created.is_some(),
        "the local daemon must keep serving after a federated peer dies"
    );

    // ── Teardown (remote already dead). ──
    drop(gui_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}

/// PR6 hardening: two GUIs federating the **same** remote target concurrently
/// converge on a single shared upstream link. The reuse check in `open_peer` is
/// not atomic with the publish across the (slow, awaiting) connect, so both opens
/// can run the full handshake before either publishes; the winner-takes-all
/// publish must leave exactly one peer with one set of words — never a leaked
/// duplicate link or a word index pointing at a connection that doesn't own it
/// (which would make the federated pane un-attachable).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn concurrent_open_peer_to_same_target_converges_on_one_link() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;

    const MARKER: &str = "CONCURRENT_OPEN_MARKER";
    let pidfile = remote_dir.path().join("shell.pid");
    let (_remote_word, shell_pid) =
        create_remote_session(&remote_token, remote_dir.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    let (gui1_tx, mut gui1_rx) = connect_authenticated(&local_token).await;
    let (gui2_tx, mut gui2_rx) = connect_authenticated(&local_token).await;

    let target = || PeerTarget::Direct {
        host: "127.0.0.1".into(),
        port: remote_tcp,
        token: remote_token.clone(),
        accept_invalid_certs: true,
    };
    // Fire BOTH opens before awaiting either reply, so the two handshakes overlap
    // and race to publish — exactly the window the fix closes.
    gui1_tx
        .send(ClientMessage::OpenPeer {
            request_id: 1,
            target: target(),
        })
        .expect("gui1 OpenPeer");
    gui2_tx
        .send(ClientMessage::OpenPeer {
            request_id: 1,
            target: target(),
        })
        .expect("gui2 OpenPeer");

    let is_peer_reply = |m: &ServerMessage| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    };
    // Disjoint mutable borrows (gui1_rx vs gui2_rx), so both futures poll under one
    // `join!` and the two opens are genuinely in flight at once.
    let (r1, r2) = tokio::join!(
        recv_until(&mut gui1_rx, Duration::from_secs(15), is_peer_reply),
        recv_until(&mut gui2_rx, Duration::from_secs(15), is_peer_reply),
    );
    let peer_of = |r: Option<ServerMessage>| match r {
        Some(ServerMessage::PeerOpened { peer, .. }) => peer,
        other => panic!("both concurrent opens must succeed, got {other:?}"),
    };
    let (p1, p2) = (peer_of(r1), peer_of(r2));
    assert_eq!(
        p1, p2,
        "both GUIs must converge on the same peer id for the same target"
    );

    // Exactly one federated session is registered (a leaked duplicate would draw a
    // second word and list a second proxied session).
    gui1_tx
        .send(ClientMessage::SessionList { request_id: 50 })
        .expect("send SessionList");
    let list = recv_until(&mut gui1_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionListResult { .. })
    })
    .await
    .expect("expected a SessionListResult");
    let federated: Vec<_> = match list {
        ServerMessage::SessionListResult { sessions, .. } => sessions
            .into_iter()
            .filter(|e| e.meta.name.contains('@'))
            .collect(),
        _ => unreachable!(),
    };
    assert_eq!(
        federated.len(),
        1,
        "a concurrent open must leave exactly one proxied session, not a leaked duplicate"
    );

    // The surviving session is attachable — proves the word index still maps to the
    // live connection (a race that overwrote the published peer would orphan it and
    // the attach would never produce a snapshot).
    let local_pane = federated[0].panes[0].pane_id.clone();
    gui1_tx
        .send(ClientMessage::Attach {
            pane_id: local_pane.clone(),
            last_seqno: None,
            size: ATTACH_SIZE,
        })
        .expect("send Attach");
    let want = local_pane.clone();
    assert!(
        recv_until(&mut gui1_rx, Duration::from_secs(15), move |m| {
            matches!(m, ServerMessage::TerminalSnapshot { pane_id, snapshot, .. }
                if *pane_id == want && snapshot_text(snapshot).contains(MARKER))
        })
        .await
        .is_some(),
        "the surviving federated pane must stay attachable after a concurrent open race"
    );

    // ── Teardown. ──
    drop(gui1_tx);
    drop(gui2_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
    set_xdg(remote_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}

/// PR6 hardening: an upstream peer link that **rejects authentication** surfaces
/// cleanly as `PeerError` (not a hang or a half-open peer), and the local daemon
/// keeps serving. This exercises the same `open_peer` branch a protocol-version
/// mismatch hits — the remote rejects `Auth` with `AuthResult { success: false }`
/// whether the cause is a bad token or a disjoint protocol range, and the range
/// guard (`dispatch::handle_message`) is checked *before* the token — so a
/// wrong token is a faithful, deterministic stand-in for the version-mismatch path
/// (which cannot be provoked without building a second daemon with a disjoint
/// supported range).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn federation_surfaces_upstream_auth_rejection_as_peer_error() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let cleanup = Cleanup::default();

    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // Remote daemon (real token), then the local hub.
    set_xdg(remote_dir.path());
    let remote_pid = spawn_daemon(&exe).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon()
        .await
        .expect("remote daemon status");
    let remote_tcp = remote_status.tcp_port;

    set_xdg(local_dir.path());
    let local_pid = spawn_daemon(&exe).await;
    cleanup.track(local_pid as i32);
    let local_token = kmux_client::daemon::query_daemon()
        .await
        .expect("local daemon status")
        .token;

    // GUI federates with a WRONG token — the remote rejects authentication, the
    // same `AuthResult { success: false }` a version mismatch produces.
    let (gui_tx, mut gui_rx) = connect_authenticated(&local_token).await;
    gui_tx
        .send(ClientMessage::OpenPeer {
            request_id: 1,
            target: PeerTarget::Direct {
                host: "127.0.0.1".into(),
                port: remote_tcp,
                token: "definitely-not-the-remote-token".into(),
                accept_invalid_certs: true,
            },
        })
        .expect("send OpenPeer");
    let reply = recv_until(&mut gui_rx, Duration::from_secs(15), |m| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    })
    .await
    .expect("expected a peer reply");
    match reply {
        ServerMessage::PeerError { reason, .. } => {
            assert!(
                reason.contains("authentication") || reason.contains("token"),
                "the rejection reason should name the auth failure, got: {reason}"
            );
        }
        other => panic!("a rejected peer must surface as PeerError, got {other:?}"),
    }

    // Isolation: the local daemon is unaffected by the rejected peer — it still
    // serves new local sessions.
    gui_tx
        .send(ClientMessage::SessionCreate {
            request_id: 2,
            name: Some("local-after-reject".into()),
            peer: None,
            cwd: Some(local_dir.path().display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exec sleep 600".into()],
            size: ATTACH_SIZE,
        })
        .expect("send local SessionCreate");
    assert!(
        recv_until(&mut gui_rx, Duration::from_secs(10), |m| {
            matches!(m, ServerMessage::SessionCreated { .. })
        })
        .await
        .is_some(),
        "the local daemon must keep serving after a peer link is rejected"
    );

    // ── Teardown. ──
    drop(gui_tx);
    set_xdg(local_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
    set_xdg(remote_dir.path());
    let _ = kmux_client::daemon::stop_daemon().await;
}
