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
    ClientCapabilities, ClientMessage, PeerTarget, ServerMessage, TermSize,
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
    let auth = recv_until(&mut srv_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::AuthResult { success: true, .. })
    })
    .await;
    assert!(auth.is_some(), "expected a successful AuthResult");
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
    let sessions = match list {
        ServerMessage::SessionListResult { sessions, .. } => sessions,
        _ => unreachable!(),
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
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let sessions = match list {
        ServerMessage::SessionListResult { sessions, .. } => sessions,
        _ => unreachable!(),
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
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
