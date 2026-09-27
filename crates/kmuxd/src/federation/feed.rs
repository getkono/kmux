//! Feeding one upstream link into the hub: answer and send pings, translate
//! each frame's IDs from remote to local, and route it — pane content to
//! that pane's viewers (feeding the mirror), session events to every viewer
//! under the session, replies to the request waiting for them, and changes
//! to the peer's sessions into the hub's own listing (issue #208).

use std::sync::{Arc, Mutex, Weak};

use kmux_protocol::messages::{
    ClientMessage, PaneProcesses, PeerId, ServerMessage, SessionEntry, SessionEventMsg,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::link::{LinkEnd, UPSTREAM_PING_INTERVAL, broadcast_sessions, upstream_silent};
use super::translate::{msg_pane_id, rewrite_event_to_local, set_msg_pane_id};
use super::{PeerConnection, lock};
use crate::app::ServerApp;

/// Feed `server_rx` into the hub until the link ends, pinging the peer every
/// [`UPSTREAM_PING_INTERVAL`] and ending the link once it has been silent too
/// long.
pub(super) async fn feed(
    app: &Weak<ServerApp>,
    conn: &Arc<Mutex<PeerConnection>>,
    peer_id: &PeerId,
    server_rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
) -> LinkEnd {
    let mut last_inbound = Instant::now();
    let mut ping = tokio::time::interval_at(
        Instant::now() + UPSTREAM_PING_INTERVAL,
        UPSTREAM_PING_INTERVAL,
    );
    let mut seq = 0u64;
    loop {
        tokio::select! {
            msg = server_rx.recv() => {
                let Some(msg) = msg else {
                    return LinkEnd::Closed;
                };
                last_inbound = Instant::now();
                if on_upstream(app, conn, peer_id, msg) == Next::Resync {
                    broadcast_sessions(app).await;
                }
            }
            _ = ping.tick() => {
                if upstream_silent(last_inbound, Instant::now()) {
                    return LinkEnd::Silent;
                }
                send_upstream(conn, ClientMessage::Ping { seq });
                seq += 1;
            }
        }
    }
}

/// What the feed loop does after a frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    Continue,
    /// The peer's sessions changed: send every client the session list.
    Resync,
}

/// Send `msg` on the link, if it is up.
fn send_upstream(conn: &Mutex<PeerConnection>, msg: ClientMessage) {
    let _ = lock(conn).client_tx.send(msg);
}

/// Route one upstream frame.
pub(super) fn on_upstream(
    app: &Weak<ServerApp>,
    conn: &Arc<Mutex<PeerConnection>>,
    peer_id: &PeerId,
    msg: ServerMessage,
) -> Next {
    match msg {
        // Keep the peer's view of us live.
        ServerMessage::Ping { seq } => {
            send_upstream(conn, ClientMessage::Pong { seq });
            Next::Continue
        }
        ServerMessage::ProcessOverviewResult { request_id, panes } => {
            on_process_overview(conn, request_id, &panes);
            Next::Continue
        }
        ServerMessage::ClientListResult {
            request_id,
            clients,
            ..
        } => {
            if let Some(tx) = lock(conn).pending_client_lists.remove(&request_id) {
                let _ = tx.send(clients);
            }
            Next::Continue
        }
        // Any session list is the peer's whole truth: an answer to the hub's
        // own refresh, or the peer's resync after the link lagged.
        ServerMessage::SessionListResult { sessions, .. } => {
            on_session_list(app, conn, peer_id, sessions)
        }
        msg => on_reply_or_stream(app, conn, msg),
    }
}

/// Route a process-overview reply (issue #122) to its waiting collector,
/// translating each pane id remote→local; panes whose word is no longer
/// mapped are dropped.
fn on_process_overview(conn: &Mutex<PeerConnection>, request_id: u64, panes: &[PaneProcesses]) {
    let mut guard = lock(conn);
    if let Some(tx) = guard.pending_overviews.remove(&request_id) {
        let localized: Vec<PaneProcesses> = panes
            .iter()
            .filter_map(|p| {
                Some(PaneProcesses {
                    pane_id: guard.to_local_pane(&p.pane_id)?,
                    ..p.clone()
                })
            })
            .collect();
        drop(guard);
        let _ = tx.send(localized);
    }
}

/// Take in the peer's session list.
fn on_session_list(
    app: &Weak<ServerApp>,
    conn: &Arc<Mutex<PeerConnection>>,
    peer_id: &PeerId,
    sessions: Vec<SessionEntry>,
) -> Next {
    let Some(app) = app.upgrade() else {
        return Next::Continue;
    };
    app.peer_manager
        .reconcile_sessions(&app, conn, peer_id, sessions);
    Next::Resync
}

/// A reply to a hub-initiated request, or a frame of the peer's stream.
fn on_reply_or_stream(
    app: &Weak<ServerApp>,
    conn: &Arc<Mutex<PeerConnection>>,
    msg: ServerMessage,
) -> Next {
    let Some(msg) = complete_request(conn, msg) else {
        return Next::Continue;
    };
    if msg_pane_id(&msg).is_some() {
        on_pane_frame(conn, msg);
        return Next::Continue;
    }
    on_session_frame(app, conn, msg)
}

/// Complete the hub request `msg` answers, if one is waiting — `None` then —
/// or hand `msg` back. `SessionCreated` carries the *remote* entry;
/// `create_remote_session` (holding `&ServerApp`) draws the local word and
/// registers it. `ClientKicked`/`SessionClosed`/`TabClosed`/`Error` complete a
/// pending acknowledgement.
fn complete_request(conn: &Mutex<PeerConnection>, msg: ServerMessage) -> Option<ServerMessage> {
    let rid = match &msg {
        ServerMessage::SessionCreated { request_id, .. }
        | ServerMessage::ClientKicked { request_id, .. }
        | ServerMessage::SessionClosed { request_id, .. }
        | ServerMessage::TabClosed { request_id, .. }
        | ServerMessage::Error {
            request_id: Some(request_id),
            ..
        } => *request_id,
        _ => return Some(msg),
    };
    // A pending ack (kick, session/tab close) takes priority for its id, then
    // a pending create.
    let ack_tx = lock(conn).pending_acks.remove(&rid);
    if let Some(tx) = ack_tx {
        let result = match msg {
            ServerMessage::Error { message, .. } => Err(message),
            _ => Ok(()),
        };
        let _ = tx.send(result);
        return None;
    }
    let create_tx = lock(conn).pending_creates.remove(&rid);
    let Some(tx) = create_tx else {
        return Some(msg);
    };
    let result = match msg {
        ServerMessage::SessionCreated { entry, .. } => Ok(entry),
        ServerMessage::Error { message, .. } => Err(message),
        other => return Some(other),
    };
    let _ = tx.send(result);
    None
}

/// A pane-scoped frame: translate the pane ID remote→local, feed the pane's
/// mirror (so a late local attacher can be served from it), then fan out to
/// that pane's viewers. `fan_out` applies the relay's backpressure policy: a
/// full viewer is sent `Lagged` (out-of-band) and dropped, then resyncs on
/// re-attach from the just-updated mirror.
fn on_pane_frame(conn: &Mutex<PeerConnection>, mut msg: ServerMessage) {
    let Some(remote_pane) = msg_pane_id(&msg).map(str::to_string) else {
        return;
    };
    let mut guard = lock(conn);
    if let Some(local_pane) = guard.to_local_pane(&remote_pane) {
        set_msg_pane_id(&mut msg, local_pane.clone());
        if let Some(pane) = guard.panes.get_mut(&local_pane) {
            pane.apply_to_mirror(&msg);
            pane.fan_out(&local_pane, &msg);
        }
    }
}

/// A session-scoped frame (titles, layout, tab/session lifecycle).
///
/// Translated remote→local and fanned out to every viewer under the session,
/// on the lanes the local daemon uses (`forward_vt_event`): layout and
/// lifecycle on the viewers' control lane, so a backed-up pane stream can never
/// drop one, and a flood-prone bell/title/progress event on the data lane,
/// dropped when congested (issue #206).
///
/// It also keeps the hub's listing current (issue #208): a layout update
/// patches the cached tab, a session the peer closed is closed here too — for
/// every client, not just its viewers — and a session or tab created, closed
/// or renamed asks the peer for its list again.
fn on_session_frame(
    app: &Weak<ServerApp>,
    conn: &Mutex<PeerConnection>,
    mut msg: ServerMessage,
) -> Next {
    if let ServerMessage::Event {
        event: SessionEventMsg::SessionClosed { word_id },
    } = &msg
    {
        on_remote_session_closed(app, conn, word_id);
        return Next::Continue;
    }
    let refresh = matches!(
        &msg,
        ServerMessage::Event {
            event: SessionEventMsg::SessionCreated { .. }
                | SessionEventMsg::SessionRenamed { .. }
                | SessionEventMsg::TabCreated { .. }
                | SessionEventMsg::TabClosed { .. }
        }
    );
    let viewers = {
        let mut guard = lock(conn);
        match &mut msg {
            ServerMessage::Event { event } => rewrite_event_to_local(event, &guard.remote_to_local)
                .map(|local_word| guard.viewers_under_word(&local_word)),
            ServerMessage::LayoutUpdate {
                word_id,
                tab_index,
                layout,
                focused_pane,
            } => guard
                .remote_to_local
                .get(word_id.as_str())
                .cloned()
                .map(|local_word| {
                    guard.cache_layout(&local_word, *tab_index, layout, *focused_pane);
                    *word_id = local_word.clone();
                    guard.viewers_under_word(&local_word)
                }),
            _ => None,
        }
    };
    for tx in viewers.unwrap_or_default() {
        crate::client_handler::forward_vt_event(&tx, msg.clone());
    }
    if refresh {
        request_session_list(conn);
    }
    Next::Continue
}

/// Ask the peer for its session list; the answer reconciles the hub's.
fn request_session_list(conn: &Mutex<PeerConnection>) {
    let mut guard = lock(conn);
    let request_id = guard.next_rid();
    let _ = guard
        .client_tx
        .send(ClientMessage::SessionList { request_id });
}

/// The peer closed its session `remote_word`: close it here for every client.
fn on_remote_session_closed(
    app: &Weak<ServerApp>,
    conn: &Mutex<PeerConnection>,
    remote_word: &str,
) {
    let Some(app) = app.upgrade() else {
        return;
    };
    let _membership = app.peer_manager.membership();
    let Some(local_word) = lock(conn).remote_to_local.get(remote_word).cloned() else {
        return;
    };
    app.peer_manager.unregister_session(&app, &local_word);
    app.broadcast_session_event(SessionEventMsg::SessionClosed {
        word_id: local_word,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kmux_protocol::messages::{LayoutNode, SplitDir, TabInfo, format_pane_id};
    use tokio::sync::broadcast;

    use super::*;
    use crate::federation::link::testing::{WAIT, listed, next_upstream};
    use crate::federation::sample_remote_entry;
    use crate::fixtures::fixture_app;

    /// The next broadcast `want` picks out.
    async fn broadcast_matching<T>(
        rx: &mut broadcast::Receiver<ServerMessage>,
        want: impl Fn(&ServerMessage) -> Option<T>,
    ) -> T {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(found) = want(&rx.recv().await.expect("open")) {
                    return found;
                }
            }
        })
        .await
        .expect("the broadcast within the bound")
    }

    fn closed_word(msg: &ServerMessage) -> Option<String> {
        match msg {
            ServerMessage::Event {
                event: SessionEventMsg::SessionClosed { word_id },
            } => Some(word_id.clone()),
            _ => None,
        }
    }

    /// A session the peer closes is closed on the hub too — for every client,
    /// not just its viewers — and forgotten, its word back in the pool
    /// (issues #202, #208).
    #[tokio::test(start_paused = true)]
    async fn a_session_the_peer_closes_is_closed_for_every_client() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let local = app.draw_word().expect("a word");
        let pool = app.available_words();
        let (_upstream, peer) = app.install_channel_peer(&local, "fedremote");

        peer.send(ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "fedremote".to_string(),
            },
        })
        .unwrap();

        assert_eq!(
            broadcast_matching(&mut broadcasts, closed_word).await,
            local
        );
        assert!(!app.is_federated_session(&local));
        assert!(app.list_federated_sessions().is_empty());
        assert_eq!(app.available_words(), pool + 1, "the word is returned");
    }

    /// A viewer of the proxied pane `fedlocal/0` on the peer
    /// `install_channel_peer` opened: its data lane and its connection.
    fn attach_viewer(
        app: &ServerApp,
    ) -> (mpsc::Receiver<ServerMessage>, crate::outbound::OutboundRx) {
        let (data_tx, data_rx) = mpsc::channel(8);
        let (ctrl_tx, ctrl_rx) = crate::fixtures::make_outbound();
        let size = kmux_protocol::messages::TermSize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        assert!(app.federated_attach(
            &format_pane_id("fedlocal", 0),
            kmux_protocol::messages::ClientId(1),
            data_tx,
            ctrl_tx,
            size,
        ));
        (data_rx, ctrl_rx)
    }

    /// A pane's frame reaches that pane's viewer under the local pane id.
    #[tokio::test(start_paused = true)]
    async fn a_pane_frame_reaches_its_viewer_under_the_local_pane_id() {
        let app = Arc::new(fixture_app());
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let (mut data, _conn) = attach_viewer(&app);
        peer.send(ServerMessage::SyncReset {
            pane_id: format_pane_id("fedremote", 0),
        })
        .unwrap();
        let got = tokio::time::timeout(WAIT, data.recv())
            .await
            .expect("the frame within the bound");
        assert!(
            matches!(&got, Some(ServerMessage::SyncReset { pane_id }) if *pane_id == "fedlocal/0"),
            "{got:?}"
        );
    }

    /// A session's events reach every viewer under it, translated to local
    /// ids: a rename on the control lane, a bell on the data lane.
    #[tokio::test(start_paused = true)]
    async fn a_session_event_reaches_its_viewers_under_local_ids() {
        let app = Arc::new(fixture_app());
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let (_data, mut conn) = attach_viewer(&app);
        for event in [
            SessionEventMsg::SessionRenamed {
                word_id: "fedremote".to_string(),
                new_name: "work".to_string(),
            },
            SessionEventMsg::PaneBell {
                pane_id: format_pane_id("fedremote", 0),
            },
        ] {
            peer.send(ServerMessage::Event { event }).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 2 {
            let msg = tokio::time::timeout(WAIT, conn.recv())
                .await
                .expect("the events within the bound")
                .expect("open");
            if let ServerMessage::Event { event } = msg {
                got.push(event);
            }
        }
        assert!(matches!(
            &got[0],
            SessionEventMsg::SessionRenamed { word_id, new_name }
                if word_id == "fedlocal" && new_name == "work"
        ));
        assert!(matches!(
            &got[1],
            SessionEventMsg::PaneBell { pane_id } if pane_id == "fedlocal/0"
        ));
    }

    /// A process overview reply completes its request with local pane ids,
    /// dropping any pane of a session the hub does not know.
    #[tokio::test(start_paused = true)]
    async fn a_process_overview_reply_is_localized_for_its_request() {
        let app = Arc::new(fixture_app());
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let (tx, rx) = tokio::sync::oneshot::channel();
        Arc::clone(&app.peer_manager.peers.lock().unwrap()["peer:1"])
            .lock()
            .unwrap()
            .pending_overviews
            .insert(9, tx);
        let pane = |pane_id: String| PaneProcesses {
            pane_id,
            root_pid: Some(1),
            processes: Vec::new(),
        };
        peer.send(ServerMessage::ProcessOverviewResult {
            request_id: 9,
            panes: vec![
                pane(format_pane_id("fedremote", 0)),
                pane(format_pane_id("unknown", 0)),
            ],
        })
        .unwrap();
        let panes = tokio::time::timeout(WAIT, rx)
            .await
            .expect("the reply within the bound")
            .expect("completed");
        let ids: Vec<_> = panes.into_iter().map(|p| p.pane_id).collect();
        assert_eq!(ids, vec!["fedlocal/0".to_string()]);
    }

    /// Close `fedlocal` from the hub while the peer answers with its ack and
    /// its own `SessionClosed` event first, then the ack, when
    /// `event_first`. Returns every `SessionClosed` word broadcast by then.
    async fn close_federated(event_first: bool) -> Vec<String> {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let (mut upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let closing = tokio::spawn({
            let app = Arc::clone(&app);
            async move { app.close_federated_session("fedlocal").await }
        });
        let request_id = next_upstream(&mut upstream, |m| match m {
            ClientMessage::SessionClose { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .await;
        let event = ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "fedremote".to_string(),
            },
        };
        let ack = ServerMessage::SessionClosed {
            request_id,
            word_id: "fedremote".to_string(),
            exit_code: None,
        };
        // Each frame is taken in whole before the next is sent, so with the
        // ack first `close_remote_session` itself closes the session.
        let settle = || tokio::time::sleep(std::time::Duration::from_millis(10));
        if event_first {
            peer.send(event).unwrap();
            settle().await;
            assert!(!app.is_federated_session("fedlocal"), "the event closed it");
            peer.send(ack).unwrap();
            closing.await.unwrap().expect("closed");
        } else {
            peer.send(ack).unwrap();
            closing.await.unwrap().expect("closed");
            assert!(!app.is_federated_session("fedlocal"), "the close did");
            peer.send(event).unwrap();
            settle().await;
        }
        let mut closed = Vec::new();
        while let Ok(msg) = broadcasts.try_recv() {
            closed.extend(closed_word(&msg));
        }
        closed
    }

    /// Closing a federated session from the hub tells every client once,
    /// whichever of the peer's ack and its own `SessionClosed` event lands
    /// first — the second finds the session already gone (issue #208).
    #[tokio::test(start_paused = true)]
    async fn closing_a_federated_session_tells_every_client_once() {
        assert_eq!(close_federated(false).await, vec!["fedlocal".to_string()]);
        assert_eq!(close_federated(true).await, vec!["fedlocal".to_string()]);
    }

    /// A session list is published while no peer's sessions can change, so a
    /// list taken before a close cannot reach clients after its
    /// `SessionClosed` and bring the session back (issue #208).
    #[tokio::test]
    async fn a_session_list_is_published_while_peer_membership_is_held() {
        let app = Arc::new(fixture_app());
        let (_upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");
        let (listed, gate_held) = app.publish_federated_sessions(|federated| {
            (
                federated.len(),
                app.peer_manager.membership.try_lock().is_err(),
            )
        });
        assert_eq!((listed, gate_held), (1, true));
        assert!(app.peer_manager.membership.try_lock().is_ok(), "released");
    }

    /// The peer's session list is its whole truth: a session it still lists
    /// keeps its word (and takes the new entry), a new one gets a word, one it
    /// no longer lists is closed — and every client is sent the result.
    #[tokio::test(start_paused = true)]
    async fn a_peer_session_list_reconciles_the_hub_listing() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");

        let mut renamed = sample_remote_entry("fedremote");
        renamed.meta.name = "renamed".to_string();
        peer.send(ServerMessage::SessionListResult {
            request_id: 7,
            sessions: vec![renamed, sample_remote_entry("newremote")],
        })
        .unwrap();
        let words = broadcast_matching(&mut broadcasts, listed).await;
        assert_eq!(words.len(), 2);
        assert!(words.contains(&("fedlocal".to_string(), false)));
        let sessions = app.list_federated_sessions();
        let kept = sessions
            .iter()
            .find(|e| e.meta.word_id == "fedlocal")
            .expect("kept under its word");
        assert_eq!(kept.meta.name, "renamed @ peer:1");
        let new_word = sessions
            .iter()
            .map(|e| e.meta.word_id.clone())
            .find(|w| w != "fedlocal")
            .expect("the new session");
        assert!(app.is_federated_session(&new_word));

        peer.send(ServerMessage::SessionListResult {
            request_id: 8,
            sessions: vec![sample_remote_entry("newremote")],
        })
        .unwrap();
        assert_eq!(
            broadcast_matching(&mut broadcasts, closed_word).await,
            "fedlocal"
        );
        assert!(!app.is_federated_session("fedlocal"));
        assert!(app.is_federated_session(&new_word));
    }

    /// A layout update from the peer patches the hub's cached tab, so the next
    /// session list shows the layout the viewers already have.
    #[tokio::test(start_paused = true)]
    async fn a_layout_update_refreshes_the_cached_tab() {
        let app = Arc::new(fixture_app());
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        let split = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![LayoutNode::single(0), LayoutNode::single(1)],
        };
        peer.send(ServerMessage::LayoutUpdate {
            word_id: "fedremote".to_string(),
            tab_index: 0,
            layout: split.clone(),
            focused_pane: 1,
        })
        .unwrap();

        let tab: TabInfo = tokio::time::timeout(WAIT, async {
            loop {
                let tab = app.list_federated_sessions()[0].tabs[0].clone();
                if tab.layout == split {
                    return tab;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the cache follows the peer");
        assert_eq!(tab.focused_pane, 1);
    }

    /// A tab or session created, closed or renamed on the peer makes the hub
    /// ask the peer for its session list again.
    #[tokio::test(start_paused = true)]
    async fn a_lifecycle_event_asks_the_peer_for_its_list() {
        let app = Arc::new(fixture_app());
        let (mut upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        peer.send(ServerMessage::Event {
            event: SessionEventMsg::TabCreated {
                word_id: "fedremote".to_string(),
                tab_index: 1,
            },
        })
        .unwrap();
        next_upstream(&mut upstream, |m| {
            matches!(m, ClientMessage::SessionList { .. }).then_some(())
        })
        .await;
    }

    /// Closing a peer closes its sessions for every client.
    #[tokio::test(start_paused = true)]
    async fn closing_a_peer_closes_its_sessions_for_every_client() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let (_upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");
        app.close_peer("peer:1");
        assert_eq!(
            broadcast_matching(&mut broadcasts, closed_word).await,
            "fedlocal"
        );
        assert!(app.list_federated_sessions().is_empty());
    }
}
