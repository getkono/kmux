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
//!
//! Every test starts from [`Federation::spawn_pair`] (the two sandboxed
//! daemons) and, where the link is expected to open, [`Federation::open_peer`].

#![cfg(all(unix, feature = "federation"))]

mod harness;

use std::path::Path;

use harness::{
    Client, E2E_TIMEOUT, Federation, SIZE as ATTACH_SIZE, connect_client, poll_until,
    read_pid_file, recv_until, type_into,
};
use kmux_protocol::messages::{
    ClientMessage, GridSnapshot, ServerMessage, SessionEntry, SessionEventMsg, TermSize,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// On the remote daemon, create a session named `marker` whose pane prints
/// `marker` then `exec`s an interactive shell (so it both shows the marker in
/// its grid and executes typed input). Records the shell's PID to `pidfile`
/// and hands it to the fixture's cleanup. Returns the remote word id.
async fn create_remote_session(fed: &Federation, marker: &str, pidfile: &Path) -> String {
    let mut remote = connect_client(&fed.remote, &fed.remote_token).await;
    let script = format!("echo $$ > {}; echo {marker}; exec sh", pidfile.display());
    let create = ClientMessage::SessionCreate {
        request_id: 1,
        name: Some(marker.into()),
        peer: None,
        cwd: Some(fed.remote.path().display().to_string()),
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), script],
        size: ATTACH_SIZE,
    };
    let word_id = ask(&mut remote, create, |m| match m {
        ServerMessage::SessionCreated { entry, .. } => Some(entry.meta.word_id.clone()),
        _ => None,
    })
    .await;
    let pid = read_pid_file(pidfile, E2E_TIMEOUT)
        .await
        .expect("shell wrote its PID");
    fed.cleanup.track(pid);
    // The remote keeps the session alive once this client drops.
    word_id
}

/// Flatten a grid snapshot's cells into a string (row-major) for marker scanning.
fn snapshot_text(snapshot: &GridSnapshot) -> String {
    snapshot.cells.iter().map(|c| c.c).collect()
}

/// Send `request` as `gui` and return the first thing `want` picks out of
/// what the daemon sends back.
async fn ask<T>(
    gui: &mut Client,
    request: ClientMessage,
    want: impl Fn(&ServerMessage) -> Option<T>,
) -> T {
    let what = format!("{request:?}");
    gui.tx.send(request).expect("send to the hub");
    let found = tokio::time::timeout(E2E_TIMEOUT, async {
        loop {
            let msg = gui.rx.recv().await?;
            if let Some(found) = want(&msg) {
                return Some(found);
            }
        }
    })
    .await
    .ok()
    .flatten();
    // `assert!` rather than `panic!`: the ratchet counts bare panics.
    assert!(
        found.is_some(),
        "no answer to {what} within {E2E_TIMEOUT:?}"
    );
    found.unwrap()
}

/// The hub's session list, as the answer to this request: a list the hub
/// broadcasts on its own (request id 0) may predate the change a test waits on.
async fn hub_list(gui: &mut Client) -> Vec<SessionEntry> {
    ask(
        gui,
        ClientMessage::SessionList { request_id: 100 },
        |m| match m {
            ServerMessage::SessionListResult {
                request_id: 100,
                sessions,
            } => Some(sessions.clone()),
            _ => None,
        },
    )
    .await
}

/// The hub's proxied sessions: those named `<name> @ <peer>`.
async fn federated_sessions(gui: &mut Client) -> Vec<SessionEntry> {
    let mut sessions = hub_list(gui).await;
    sessions.retain(|e| e.meta.name.contains(" @ "));
    sessions
}

/// The first pane of the one session the hub proxies.
async fn federated_pane(gui: &mut Client) -> String {
    let sessions = federated_sessions(gui).await;
    assert_eq!(sessions.len(), 1, "one proxied session: {sessions:?}");
    sessions[0].panes[0].pane_id.clone()
}

/// The hub's local word for the remote session named `name` in `sessions`.
fn word_named(sessions: &[SessionEntry], name: &str) -> Option<String> {
    let prefix = format!("{name} @ ");
    sessions
        .iter()
        .find(|e| e.meta.name.starts_with(&prefix))
        .map(|e| e.meta.word_id.clone())
}

/// Attach `gui` to `pane` at `size` and return the first snapshot of it.
async fn attach_snapshot(gui: &mut Client, pane: &str, size: TermSize) -> GridSnapshot {
    let attach = ClientMessage::Attach {
        pane_id: pane.to_string(),
        last_seqno: None,
        size,
    };
    ask(gui, attach, |m| match m {
        ServerMessage::TerminalSnapshot {
            pane_id, snapshot, ..
        } if pane_id == pane => Some((**snapshot).clone()),
        _ => None,
    })
    .await
}

/// The hub keeps serving its own sessions: a GUI can still create one.
async fn assert_hub_still_serves(fed: &Federation, gui: &mut Client) {
    let create = ClientMessage::SessionCreate {
        request_id: 2,
        name: Some("local".into()),
        peer: None,
        cwd: Some(fed.local.path().display().to_string()),
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), "exec sleep 600".into()],
        size: ATTACH_SIZE,
    };
    let created = ask(gui, create, |m| match m {
        ServerMessage::SessionCreated { entry, .. } => Some(entry.peer.clone()),
        _ => None,
    })
    .await;
    assert_eq!(created, None, "a local session, on the hub itself");
}

/// The headline #121 path: one GUI attaches to a remote session *through* the
/// local daemon over a single federated link, and both input and output flow.
#[tokio::test]
async fn gui_attaches_to_remote_session_through_local_daemon() {
    const MARKER: &str = "FEDMARKER_OUTPUT";
    let fed = Federation::spawn_pair().await;
    let pidfile = fed.remote.path().join("shell.pid");
    let remote_word = create_remote_session(&fed, MARKER, &pidfile).await;

    // A mock GUI federates the local daemon to the remote over a direct
    // TCP+TLS endpoint.
    let (mut gui, peer) = fed.open_peer().await;
    assert_eq!(peer, format!("127.0.0.1:{}", fed.remote_tcp));

    // The remote's session appears in the hub's list under a *local* word
    // and a peer-decorated name, its panes namespaced under that word.
    let sessions = federated_sessions(&mut gui).await;
    let [entry] = sessions.as_slice() else {
        panic!("one proxied session: {sessions:?}");
    };
    assert_ne!(entry.meta.word_id, remote_word, "a fresh local word");
    let pane = entry.panes[0].pane_id.clone();
    assert!(pane.starts_with(&entry.meta.word_id), "{pane}");

    // The remote's startup output arrives addressed by the *local* pane ID.
    let snapshot = attach_snapshot(&mut gui, &pane, ATTACH_SIZE).await;
    assert!(snapshot_text(&snapshot).contains(MARKER));

    // Input typed into the GUI runs on the *remote* PTY.
    let input_marker = fed.remote.path().join("fed_input_marker");
    type_into(&gui, &pane, &format!("touch {}\n", input_marker.display()));
    assert!(
        poll_until(E2E_TIMEOUT, || input_marker.exists()).await,
        "GUI input must reach the remote PTY and create the marker file"
    );

    // Session-scoped events propagate, translated to the local pane ID: an
    // OSC 2 title set on the remote reaches the GUI as `PaneTitleChanged`.
    type_into(&gui, &pane, "printf '\\033]2;FEDTITLE_XYZ\\007'\n");
    let title = recv_until(&mut gui.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::Event {
            event: SessionEventMsg::PaneTitleChanged { pane_id, title },
        } if *pane_id == pane && title.contains("FEDTITLE_XYZ"))
    })
    .await;
    assert!(title.is_some(), "the remote pane's title reaches the GUI");

    drop(gui);
    fed.shutdown().await;
}

/// Creating a session on a federated peer (issue #121 launcher): the GUI sends
/// `SessionCreate { peer: Some(..) }` to the hub, which forwards it upstream,
/// registers the result under a local word, and replies `SessionCreated` with the
/// session attributed to its peer. The new session must run on the *remote* host.
#[tokio::test]
async fn gui_creates_a_session_on_a_federated_peer() {
    let fed = Federation::spawn_pair().await;
    let (mut gui, peer_id) = fed.open_peer().await;

    // The shell records its PID, proving a live *remote* PTY was spawned.
    let pidfile = fed.remote.path().join("created.pid");
    let create = ClientMessage::SessionCreate {
        request_id: 20,
        name: Some("made-on-remote".into()),
        cwd: Some(fed.remote.path().display().to_string()),
        program: Some("/bin/sh".into()),
        args: vec![
            "-c".into(),
            format!("echo $$ > {}; exec sleep 600", pidfile.display()),
        ],
        size: ATTACH_SIZE,
        peer: Some(peer_id.clone()),
    };
    let entry = ask(&mut gui, create, |m| match m {
        ServerMessage::SessionCreated { entry, .. } => Some(Ok(entry.clone())),
        ServerMessage::Error { message, .. } => Some(Err(message.clone())),
        _ => None,
    })
    .await
    .expect("the remote creates it");

    // Attributed to the peer, addressed by a fresh local word.
    assert_eq!(entry.peer.as_deref(), Some(peer_id.as_str()));
    assert!(entry.meta.name.contains("made-on-remote"));
    assert!(entry.panes[0].pane_id.starts_with(&entry.meta.word_id));

    // The shell really ran on the remote host: its PID file appears there.
    let shell_pid = read_pid_file(&pidfile, E2E_TIMEOUT)
        .await
        .expect("peer-created shell must write PID");
    fed.cleanup.track(shell_pid);

    // The hub's merged list carries it, attributed to its peer.
    let listed = hub_list(&mut gui).await;
    assert!(
        listed
            .iter()
            .any(|e| e.meta.word_id == entry.meta.word_id
                && e.peer.as_deref() == Some(peer_id.as_str())),
        "{listed:?}"
    );

    drop(gui);
    fed.shutdown().await;
}

/// PR4 reconciliation: two local GUIs share **one** proxied pane over a single
/// federated link. A smaller second viewer shrinks the shared pane (smallest-wins),
/// and the late viewer is served the live mirror's content.
#[tokio::test]
async fn two_guis_share_one_proxied_pane_with_smallest_wins() {
    const MARKER: &str = "SHARED_PANE_MARKER";
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, MARKER, &fed.remote.path().join("shell.pid")).await;
    let small = TermSize {
        rows: 10,
        cols: 40,
        pixel_width: 0,
        pixel_height: 0,
    };

    // GUI-1 federates and attaches at the large size, as the sole viewer.
    let (mut gui1, _peer) = fed.open_peer().await;
    let pane = federated_pane(&mut gui1).await;
    let snapshot = attach_snapshot(&mut gui1, &pane, ATTACH_SIZE).await;
    assert_eq!(snapshot.rows, ATTACH_SIZE.rows, "the sole viewer's size");

    // GUI-2, a second connection to the same hub, sees the same pane over
    // the one open peer, and is served the live mirror's content.
    let mut gui2 = fed.connect_gui().await;
    assert_eq!(federated_pane(&mut gui2).await, pane);
    let snapshot = attach_snapshot(&mut gui2, &pane, small).await;
    assert!(snapshot_text(&snapshot).contains(MARKER));

    // Smallest-wins: GUI-2 joining shrinks the shared pane, so GUI-1 is
    // resent it at 10 rows.
    let shrunk = recv_until(&mut gui1.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::TerminalSnapshot { pane_id, snapshot, .. }
            if *pane_id == pane && snapshot.rows == small.rows)
    })
    .await;
    assert!(shrunk.is_some(), "a smaller second viewer shrinks the pane");

    drop((gui1, gui2));
    fed.shutdown().await;
}

/// PR6 hardening: when the remote daemon dies, the failure is isolated. The GUI's
/// federated session is listed as unreachable (not closed, not left hanging;
/// issue #208), and the local daemon keeps serving — proxied panes live apart
/// from locally-hosted ones.
#[tokio::test]
async fn remote_daemon_death_is_isolated_from_local_daemon() {
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, "ISO_MARKER", &fed.remote.path().join("shell.pid")).await;
    let (mut gui, _peer) = fed.open_peer().await;
    let pane = federated_pane(&mut gui).await;
    let word = pane.split('/').next().unwrap().to_string();
    attach_snapshot(&mut gui, &pane, ATTACH_SIZE).await;

    // Kill the remote daemon hard — its TCP link drops under the local daemon.
    let _ = kill(Pid::from_raw(fed.remote_pid.cast_signed()), Signal::SIGKILL);

    // The session is listed as unreachable while the hub keeps re-opening
    // the link (issue #208), and the hub keeps serving.
    assert_eq!(await_peer_state(&mut gui, &word).await, Some(true));
    assert_hub_still_serves(&fed, &mut gui).await;

    drop(gui);
    fed.shutdown().await;
}

/// The next session list the hub sends that names `local_word`: whether it
/// flags the word's peer unreachable. `None` if none arrives in time. Fails
/// if any message on the way closes `local_word` — a peer blip must not
/// (issue #208).
async fn await_peer_state(gui: &mut Client, local_word: &str) -> Option<bool> {
    let read = async {
        loop {
            match gui.rx.recv().await? {
                ServerMessage::Event {
                    event: SessionEventMsg::SessionClosed { word_id },
                } => assert_ne!(word_id, local_word, "the session must not be closed"),
                ServerMessage::SessionListResult { sessions, .. } => {
                    if let Some(entry) = sessions.iter().find(|e| e.meta.word_id == local_word) {
                        return Some(entry.peer_unreachable);
                    }
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(E2E_TIMEOUT, read).await.ok().flatten()
}

/// Issue #208: a peer whose link goes silent (here, a frozen remote daemon)
/// is called unreachable once the hub's inbound deadline passes — its
/// session listed and flagged, closed for no one. When the peer answers
/// again, the hub re-opens the link on its own: the session is back under the
/// same local word, and re-attaching its pane shows what was on screen.
///
/// Out of process because the silence is a real peer's: a frozen daemon, which
/// no channel stand-in reproduces (the in-memory tier covers the same
/// supervisor on the paused clock, in `federation::link`).
#[tokio::test]
async fn a_frozen_peer_is_unreachable_then_restored_under_the_same_word() {
    const MARKER: &str = "FEDMARKER_RESTORED";
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, MARKER, &fed.remote.path().join("shell.pid")).await;
    let (mut gui, _peer) = fed.open_peer().await;
    let pane = federated_pane(&mut gui).await;
    let word = pane.split('/').next().unwrap().to_string();

    // Freeze the remote: its link goes silent, but nothing closes it.
    let remote = Pid::from_raw(fed.remote_pid.cast_signed());
    kill(remote, Signal::SIGSTOP).expect("freeze the remote");
    let unreachable = await_peer_state(&mut gui, &word).await;
    // Thaw it before asserting, so a failure does not leave it frozen.
    kill(remote, Signal::SIGCONT).expect("thaw the remote");
    assert_eq!(unreachable, Some(true), "the silent peer is unreachable");

    assert_eq!(
        await_peer_state(&mut gui, &word).await,
        Some(false),
        "the peer comes back under the same local word"
    );
    let snapshot = attach_snapshot(&mut gui, &pane, ATTACH_SIZE).await;
    assert!(snapshot_text(&snapshot).contains(MARKER));

    drop(gui);
    fed.shutdown().await;
}

/// Issue #202: a federated session its peer closes leaves the hub's list,
/// whether a client attached straight to the peer closed it or the hub closed
/// its last tab. (A pane whose shell exits keeps its slot until someone closes
/// it, so that closes no session; a peer that is lost keeps its sessions
/// listed as unreachable, see
/// `remote_daemon_death_is_isolated_from_local_daemon`.)
///
/// Out of process because each close is the real peer's own: its close
/// handling and the `SessionClosed` it sends the hub over the link. The feed's
/// routing of that event is pinned in-memory in `federation::feed`.
#[tokio::test]
async fn a_session_its_peer_closes_leaves_the_hubs_list() {
    const PEER_CLOSES: &str = "FED_PEER_CLOSES";
    const LAST_TAB: &str = "FED_LAST_TAB";
    let fed = Federation::spawn_pair().await;
    let peer_word =
        create_remote_session(&fed, PEER_CLOSES, &fed.remote.path().join("peer.pid")).await;
    create_remote_session(&fed, LAST_TAB, &fed.remote.path().join("tab.pid")).await;
    let (mut gui, _peer) = fed.open_peer().await;
    let listed = hub_list(&mut gui).await;
    let peer_closes = word_named(&listed, PEER_CLOSES).expect("the hub lists it");
    let last_tab = word_named(&listed, LAST_TAB).expect("the hub lists it");

    // A client attached straight to the peer closes the session there.
    let remote = connect_client(&fed.remote, &fed.remote_token).await;
    let close = remote.tx.send(ClientMessage::SessionClose {
        request_id: 1,
        word_id: peer_word,
    });
    assert!(close.is_ok(), "send SessionClose to the peer");
    let listed = await_closed(&mut gui, &peer_closes).await;
    assert!(!listed.contains(&peer_closes), "{listed:?}");

    // The hub closes the session's last tab on the peer.
    let close = gui.tx.send(ClientMessage::TabClose {
        request_id: 2,
        word_id: last_tab.clone(),
        tab_index: 0,
    });
    assert!(close.is_ok(), "send TabClose to the hub");
    let listed = await_closed(&mut gui, &last_tab).await;
    assert!(!listed.contains(&last_tab), "{listed:?}");

    drop((remote, gui));
    fed.shutdown().await;
}

/// Wait for the hub to tell `gui` that `local_word` closed, then return the
/// words the hub lists after it.
async fn await_closed(gui: &mut Client, local_word: &str) -> Vec<String> {
    let closed = recv_until(&mut gui.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::Event {
            event: SessionEventMsg::SessionClosed { word_id },
        } if word_id == local_word)
    })
    .await;
    assert!(closed.is_some(), "{local_word} closes");
    hub_list(gui)
        .await
        .into_iter()
        .map(|e| e.meta.word_id)
        .collect()
}

/// A session closed through the hub is restorable through it, as on a local
/// daemon (issue #228): the hub's closed list shows it under its peer, a
/// restore naming that peer brings it back under a fresh local word, and a
/// GUI attaches to it and sees its screen.
///
/// Out of process because the graveyard, the restore's respawn and the
/// session list are the real peer's; the hub's collection and forwarding are
/// pinned in-memory in `federation::feed` and `federation::forward`.
#[tokio::test]
async fn a_peers_closed_session_is_listed_and_restored_through_the_hub() {
    const MARKER: &str = "FED_RESTORED";
    let fed = Federation::spawn_pair().await;
    let pidfile = fed.remote.path().join("restored.pid");
    create_remote_session(&fed, MARKER, &pidfile).await;
    let (mut gui, peer_id) = fed.open_peer().await;
    let listed = hub_list(&mut gui).await;
    let word = word_named(&listed, MARKER).expect("the hub lists it");

    // Close it through the hub; the peer keeps it in its graveyard.
    std::fs::remove_file(&pidfile).expect("the first shell's PID file");
    let close = gui.tx.send(ClientMessage::SessionClose {
        request_id: 1,
        word_id: word.clone(),
    });
    assert!(close.is_ok(), "send SessionClose to the hub");
    assert!(!await_closed(&mut gui, &word).await.contains(&word));

    // The hub's closed list has it, under its peer.
    let list = ClientMessage::SessionListClosed { request_id: 101 };
    let closed = ask(&mut gui, list, |m| match m {
        ServerMessage::ClosedSessionListResult {
            request_id: 101,
            sessions,
        } => sessions.iter().find(|e| e.meta.name == MARKER).cloned(),
        _ => None,
    })
    .await;
    assert_eq!(closed.peer.as_deref(), Some(peer_id.as_str()));

    // Restoring it names the peer, and it comes back under a local word.
    let restore = ClientMessage::SessionRestore {
        request_id: 102,
        word_id: closed.meta.word_id.clone(),
        peer: closed.peer.clone(),
    };
    let entry = ask(&mut gui, restore, |m| match m {
        ServerMessage::SessionCreated {
            request_id: 102,
            entry,
        } => Some(Ok(entry.clone())),
        ServerMessage::Error { message, .. } => Some(Err(message.clone())),
        _ => None,
    })
    .await
    .expect("the peer restores it");
    let shell = read_pid_file(&pidfile, E2E_TIMEOUT)
        .await
        .expect("the restored shell writes its PID");
    fed.cleanup.track(shell);
    assert_eq!(entry.peer.as_deref(), Some(peer_id.as_str()));
    let listed = hub_list(&mut gui).await;
    assert!(
        listed.iter().any(|e| e.meta.word_id == entry.meta.word_id),
        "{listed:?}"
    );

    // A GUI attaches to it through the hub and sees its screen.
    let pane = entry.panes[0].pane_id.clone();
    let screen = snapshot_text(&attach_snapshot(&mut gui, &pane, ATTACH_SIZE).await);
    assert!(screen.contains(MARKER), "{screen:?}");

    drop(gui);
    fed.shutdown().await;
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
    const MARKER: &str = "CONCURRENT_OPEN_MARKER";
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, MARKER, &fed.remote.path().join("shell.pid")).await;

    // Both opens are sent by hand rather than through `Federation::open_peer`,
    // which awaits its reply: the point is two handshakes in flight at once.
    let mut gui1 = fed.connect_gui().await;
    let mut gui2 = fed.connect_gui().await;
    for gui in [&gui1, &gui2] {
        let open = gui.tx.send(ClientMessage::OpenPeer {
            request_id: 1,
            target: fed.remote_target(&fed.remote_token),
        });
        assert!(open.is_ok(), "send OpenPeer");
    }
    let is_peer_reply = |m: &ServerMessage| {
        matches!(
            m,
            ServerMessage::PeerOpened { .. } | ServerMessage::PeerError { .. }
        )
    };
    // Disjoint mutable borrows, so both replies are awaited at once.
    let (r1, r2) = tokio::join!(
        recv_until(&mut gui1.rx, E2E_TIMEOUT, is_peer_reply),
        recv_until(&mut gui2.rx, E2E_TIMEOUT, is_peer_reply),
    );
    let peer_of = |r: Option<ServerMessage>| match r {
        Some(ServerMessage::PeerOpened { peer, .. }) => peer,
        other => panic!("both concurrent opens must succeed, got {other:?}"),
    };
    assert_eq!(peer_of(r1), peer_of(r2), "one peer id for one target");

    // Exactly one proxied session (a leaked duplicate would draw a second
    // word), and it is attachable: a race that overwrote the published peer
    // would orphan the word, and the attach would never produce a snapshot.
    let pane = federated_pane(&mut gui1).await;
    let snapshot = attach_snapshot(&mut gui1, &pane, ATTACH_SIZE).await;
    assert!(snapshot_text(&snapshot).contains(MARKER));

    drop((gui1, gui2));
    fed.shutdown().await;
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
    let fed = Federation::spawn_pair().await;
    let mut gui = fed.connect_gui().await;
    let open = ClientMessage::OpenPeer {
        request_id: 1,
        target: fed.remote_target("definitely-not-the-remote-token"),
    };
    let reason = ask(&mut gui, open, |m| match m {
        ServerMessage::PeerError { reason, .. } => Some(Ok(reason.clone())),
        ServerMessage::PeerOpened { .. } => Some(Err(format!("{m:?}"))),
        _ => None,
    })
    .await
    .expect("a rejected peer surfaces as PeerError");
    assert!(
        reason.contains("authentication") || reason.contains("token"),
        "the reason names the auth failure: {reason}"
    );
    assert_hub_still_serves(&fed, &mut gui).await;

    drop(gui);
    fed.shutdown().await;
}

/// Out of process because each request is carried out by the real peer: its
/// own handlers answer under its own ids, and the hub relays the answer under
/// the GUI's (issue #227). The routing itself is pinned in memory in
/// `federation::forward`.
#[tokio::test]
async fn a_proxied_sessions_tabs_and_panes_are_managed_through_the_hub() {
    const MARKER: &str = "FED_MANAGED";
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, MARKER, &fed.remote.path().join("managed.pid")).await;
    let (mut gui, _peer) = fed.open_peer().await;
    let listed = hub_list(&mut gui).await;
    let word = word_named(&listed, MARKER).expect("the hub lists it");
    let sleeper = || Some("/bin/sleep".to_string());
    let long = || vec!["600".to_string()];

    let create = ClientMessage::TabCreate {
        request_id: 1,
        word_id: word.clone(),
        program: sleeper(),
        args: long(),
        size: ATTACH_SIZE,
    };
    let (tab_word, tab_index, focused) = ask(&mut gui, create, |m| match m {
        ServerMessage::TabCreated {
            request_id: 1,
            word_id,
            tab,
        } => Some((word_id.clone(), tab.tab_index, tab.focused_pane)),
        _ => None,
    })
    .await;
    assert_eq!(tab_word, word);

    let split = ClientMessage::PaneSplit {
        request_id: 2,
        word_id: word.clone(),
        tab_index,
        from_pane: focused,
        dir: kmux_protocol::messages::SplitDir::Vertical,
        program: sleeper(),
        args: long(),
        size: ATTACH_SIZE,
    };
    let (split_word, new_pane) = ask(&mut gui, split, |m| match m {
        ServerMessage::PaneSplit {
            request_id: 2,
            word_id,
            new_pane,
            ..
        } => Some((word_id.clone(), new_pane.pane_id.clone())),
        _ => None,
    })
    .await;
    assert_eq!(split_word, word);
    assert!(new_pane.starts_with(&format!("{word}/")), "{new_pane}");

    let rename = ClientMessage::TabRename {
        request_id: 3,
        word_id: word.clone(),
        tab_index,
        new_name: "work".into(),
    };
    let renamed = ask(&mut gui, rename, |m| match m {
        ServerMessage::Event {
            event: SessionEventMsg::TabRenamed { word_id, name, .. },
        } => Some((word_id.clone(), name.clone())),
        _ => None,
    })
    .await;
    assert_eq!(renamed, (word.clone(), "work".to_string()));

    // The peer's answer to a rename carries no request id: the hub routes
    // it to the client that asked by order.
    let rename = ClientMessage::SessionRename {
        request_id: 4,
        word_id: word.clone(),
        new_name: "managed".into(),
    };
    let renamed = ask(&mut gui, rename, |m| match m {
        ServerMessage::SessionRenamed { word_id, new_name } => {
            Some((word_id.clone(), new_name.clone()))
        }
        _ => None,
    })
    .await;
    assert_eq!(renamed, (word.clone(), "managed".to_string()));

    let close = ClientMessage::TabClose {
        request_id: 5,
        word_id: word.clone(),
        tab_index,
    };
    let closed = ask(&mut gui, close, |m| match m {
        ServerMessage::TabClosed {
            request_id: 5,
            word_id,
            ..
        } => Some(word_id.clone()),
        _ => None,
    })
    .await;
    assert_eq!(closed, word);

    drop(gui);
    fed.shutdown().await;
}

/// Out of process because the history is the real peer's scrollback, and
/// the answer crosses the real link. Two GUIs view the pane; the one that
/// asks is answered under its own request id, and the other never sees that
/// answer (issue #227).
#[tokio::test]
async fn a_proxied_panes_history_answers_only_the_client_that_asked() {
    const MARKER: &str = "FED_HISTORY";
    let fed = Federation::spawn_pair().await;
    create_remote_session(&fed, MARKER, &fed.remote.path().join("history.pid")).await;
    let (mut asker, _peer) = fed.open_peer().await;
    let mut other = fed.connect_gui().await;
    let pane = federated_pane(&mut asker).await;
    for gui in [&mut asker, &mut other] {
        attach_snapshot(gui, &pane, ATTACH_SIZE).await;
    }
    let history = |request_id| ClientMessage::FetchHistory {
        request_id,
        pane_id: pane.clone(),
        start_index: 0,
        count: 10,
    };
    let answered = |m: &ServerMessage| match m {
        ServerMessage::HistoryLines {
            request_id,
            pane_id,
            ..
        } => Some((*request_id, pane_id.clone())),
        _ => None,
    };

    assert_eq!(
        ask(&mut asker, history(7), answered).await,
        (7, pane.clone())
    );
    // Asked second, the other GUI's first history is its own answer: the
    // asker's never reached it.
    assert_eq!(
        ask(&mut other, history(3), answered).await,
        (3, pane.clone())
    );

    drop((asker, other));
    fed.shutdown().await;
}
