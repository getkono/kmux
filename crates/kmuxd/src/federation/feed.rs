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
    let local_word = lock(conn).remote_to_local.get(remote_word).cloned();
    let (Some(app), Some(local_word)) = (app.upgrade(), local_word) else {
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

    use kmux_protocol::messages::{LayoutNode, SplitDir, TabInfo};
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
    /// not just its viewers — and forgotten (issue #208).
    #[tokio::test(start_paused = true)]
    async fn a_session_the_peer_closes_is_closed_for_every_client() {
        let app = Arc::new(fixture_app());
        let mut broadcasts = app.subscribe_vt_events();
        let (_upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");

        peer.send(ServerMessage::Event {
            event: SessionEventMsg::SessionClosed {
                word_id: "fedremote".to_string(),
            },
        })
        .unwrap();

        assert_eq!(
            broadcast_matching(&mut broadcasts, closed_word).await,
            "fedlocal"
        );
        assert!(!app.is_federated_session("fedlocal"));
        assert!(app.all_sessions().await.is_empty());
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
        let sessions = app.all_sessions().await;
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
                let tab = app.all_sessions().await[0].tabs[0].clone();
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
        assert!(app.all_sessions().await.is_empty());
    }
}
