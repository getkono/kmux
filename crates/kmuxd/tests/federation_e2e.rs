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

mod harness;

use std::path::Path;
use std::time::Duration;

use harness::{
    Cleanup, Daemon, Sandbox, connect_client, daemon_token, poll_until, read_pid_file, recv_until,
};
use kmux_protocol::messages::{
    ClientMessage, PeerTarget, ServerMessage, SessionEventMsg, TermSize,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;

const ATTACH_SIZE: TermSize = TermSize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Connect a mock client to `sandbox`'s data UDS and authenticate. Returns the
/// sink and the receiver separately, which is what lets one test drive two GUIs
/// against one hub.
async fn connect_authenticated(
    sandbox: &Sandbox,
    token: &str,
) -> (
    mpsc::UnboundedSender<ClientMessage>,
    mpsc::UnboundedReceiver<ServerMessage>,
) {
    let client = connect_client(sandbox, token).await;
    (client.tx, client.rx)
}

/// On the daemon in `sandbox`, create a session whose pane prints `marker` then
/// `exec`s an interactive shell (so it both shows the marker
/// in its grid and executes typed input). Records the shell's PID to `pidfile`.
/// Returns `(remote_word_id, shell_pid)`.
async fn create_remote_session(
    sandbox: &Sandbox,
    token: &str,
    cwd: &Path,
    marker: &str,
    pidfile: &Path,
) -> (String, i32) {
    let (client_tx, mut srv_rx) = connect_authenticated(sandbox, token).await;

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
async fn gui_attaches_to_remote_session_through_local_daemon() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    // ── Remote daemon: host a real session with a known startup marker. ──
    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    assert!(
        remote_tcp != 0,
        "remote daemon must expose an ephemeral TCP+TLS port for federation"
    );

    const MARKER: &str = "FEDMARKER_OUTPUT";
    let pidfile = remote.path().join("shell.pid");
    let (remote_word, shell_pid) =
        create_remote_session(&remote, &remote_token, remote.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    // ── Local daemon: the per-user hub the GUI actually talks to. ──
    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

    // ── Mock GUI → local daemon (UDS). ──
    let (gui_tx, mut gui_rx) = connect_authenticated(&local, &local_token).await;

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
    let input_marker = remote.path().join("fed_input_marker");
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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
    let _ = kmux_client::daemon::stop_daemon_at(&remote.socket_path()).await;
}

/// Creating a session on a federated peer (issue #121 launcher): the GUI sends
/// `SessionCreate { peer: Some(..) }` to the hub, which forwards it upstream,
/// registers the result under a local word, and replies `SessionCreated` with the
/// session attributed to its peer. The new session must run on the *remote* host.
#[tokio::test]
async fn gui_creates_a_session_on_a_federated_peer() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    // ── Remote daemon: starts with no sessions; the hub will create one on it. ──
    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    assert!(remote_tcp != 0, "remote daemon must expose a TCP+TLS port");

    // ── Local hub daemon: what the GUI talks to. ──
    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

    let (gui_tx, mut gui_rx) = connect_authenticated(&local, &local_token).await;

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
    let pidfile = remote.path().join("created.pid");
    let script = format!("echo $$ > {}; exec sleep 600", pidfile.display());
    gui_tx
        .send(ClientMessage::SessionCreate {
            request_id: 20,
            name: Some("made-on-remote".into()),
            cwd: Some(remote.path().display().to_string()),
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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
    let _ = kmux_client::daemon::stop_daemon_at(&remote.socket_path()).await;
}

/// PR4 reconciliation: two local GUIs share **one** proxied pane over a single
/// federated link. A smaller second viewer shrinks the shared pane (smallest-wins),
/// and the late viewer is served the live mirror's content.
#[tokio::test]
async fn two_guis_share_one_proxied_pane_with_smallest_wins() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    // Remote daemon hosting a marked session, then the local hub.
    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;

    const MARKER: &str = "SHARED_PANE_MARKER";
    let pidfile = remote.path().join("shell.pid");
    let (_remote_word, shell_pid) =
        create_remote_session(&remote, &remote_token, remote.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

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
    let (gui1_tx, mut gui1_rx) = connect_authenticated(&local, &local_token).await;
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
    let (gui2_tx, mut gui2_rx) = connect_authenticated(&local, &local_token).await;
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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
    let _ = kmux_client::daemon::stop_daemon_at(&remote.socket_path()).await;
}

/// PR6 hardening: when the remote daemon dies, the failure is isolated. The GUI's
/// federated session is cleanly closed (not left hanging), and the local daemon
/// keeps serving — proxied panes live apart from locally-hosted ones.
#[tokio::test]
async fn remote_daemon_death_is_isolated_from_local_daemon() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    // Remote daemon + a session, then the local hub.
    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;
    let pidfile = remote.path().join("shell.pid");
    let (_remote_word, shell_pid) = create_remote_session(
        &remote,
        &remote_token,
        remote.path(),
        "ISO_MARKER",
        &pidfile,
    )
    .await;
    cleanup.track(shell_pid);

    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

    // GUI federates and attaches to the remote session through the local daemon.
    let (gui_tx, mut gui_rx) = connect_authenticated(&local, &local_token).await;
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
            cwd: Some(local.path().display().to_string()),
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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
}

/// PR6 hardening: two GUIs federating the **same** remote target concurrently
/// converge on a single shared upstream link. The reuse check in `open_peer` is
/// not atomic with the publish across the (slow, awaiting) connect, so both opens
/// can run the full handshake before either publishes; the winner-takes-all
/// publish must leave exactly one peer with one set of words — never a leaked
/// duplicate link or a word index pointing at a connection that doesn't own it
/// (which would make the federated pane un-attachable).
#[tokio::test]
async fn concurrent_open_peer_to_same_target_converges_on_one_link() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_token = remote_status.token.clone();
    let remote_tcp = remote_status.tcp_port;

    const MARKER: &str = "CONCURRENT_OPEN_MARKER";
    let pidfile = remote.path().join("shell.pid");
    let (_remote_word, shell_pid) =
        create_remote_session(&remote, &remote_token, remote.path(), MARKER, &pidfile).await;
    cleanup.track(shell_pid);

    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

    let (gui1_tx, mut gui1_rx) = connect_authenticated(&local, &local_token).await;
    let (gui2_tx, mut gui2_rx) = connect_authenticated(&local, &local_token).await;

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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
    let _ = kmux_client::daemon::stop_daemon_at(&remote.socket_path()).await;
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
async fn federation_surfaces_upstream_auth_rejection_as_peer_error() {
    let cleanup = Cleanup::default();

    let remote = Sandbox::new();
    let local = Sandbox::new();

    // Remote daemon (real token), then the local hub.
    let remote_pid = Daemon::new(&remote).spawn(None).await;
    cleanup.track(remote_pid as i32);
    let remote_status = kmux_client::daemon::query_daemon_at(&remote.socket_path())
        .await
        .expect("remote daemon status");
    let remote_tcp = remote_status.tcp_port;

    let local_pid = Daemon::new(&local).spawn(None).await;
    cleanup.track(local_pid as i32);
    let local_token = daemon_token(&local).await;

    // GUI federates with a WRONG token — the remote rejects authentication, the
    // same `AuthResult { success: false }` a version mismatch produces.
    let (gui_tx, mut gui_rx) = connect_authenticated(&local, &local_token).await;
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
            cwd: Some(local.path().display().to_string()),
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
    let _ = kmux_client::daemon::stop_daemon_at(&local.socket_path()).await;
    let _ = kmux_client::daemon::stop_daemon_at(&remote.socket_path()).await;
}
