//! The one forwarding path for a proxied session (issue #227).
//!
//! What a hub does with each client message is declared once, in
//! [`ClientMessage::federation`]. Every *forwarded* one comes through
//! [`PeerManager::forward`]: the session or pane it names is rewritten to the
//! peer's ids, its `request_id` to one of the hub's own, and it goes up the
//! link, recorded in the link's [`Routes`](super::routes::Routes). The peer's
//! answer comes back through [`answer_routed`] (an answer with an id) or
//! [`answer_unrequested`] (one without), rewritten to the sender's ids, to
//! the sender alone. A request the hub cannot forward — the peer is down,
//! the session is gone, another viewer holds the input lock — is refused at
//! once with a typed [`Refusal`]; one in flight when the link drops is
//! answered by [`fail_in_flight`]. Nothing is dropped unanswered.
//!
//! The *aggregated* input lock is here too: the hub arbitrates its own
//! viewers and holds the peer's lock for the one holding its own.

use std::sync::{Arc, Mutex, Weak};

use kmux_protocol::messages::{
    ClientMessage, ErrorCode, Federation, LayoutScheme, PeerId, ServerMessage, SessionEntry, Target,
};
use kmux_protocol::parse_pane_id;
use tracing::debug;

use super::routes::{Requester, Route};
use super::translate::{localize_reply, reply_request_id, reply_word, set_reply_request_id};
use super::{PeerConnection, PeerManager, lock};
use crate::app::Refusal;
use crate::app::ServerApp;

/// Whether `msg` is one of the three that write to a pane: the ones another
/// viewer's input lock refuses.
fn writes_input(msg: &ClientMessage) -> bool {
    matches!(
        msg,
        ClientMessage::PtyInput { .. }
            | ClientMessage::PtyKeyBatch { .. }
            | ClientMessage::PtyPaste { .. }
    )
}

/// Make `msg` fit to send on, or say it is not to be sent: the hub never
/// sends `Unknown`. A newer client's attention kind is passed on as the one
/// it is shown as; a layout scheme this hub does not know is dropped, as a
/// local daemon ignores it.
fn sendable(msg: &mut ClientMessage) -> bool {
    match msg {
        ClientMessage::Notify { kind, .. } => {
            *kind = kind.sendable();
            true
        }
        ClientMessage::ApplyLayoutScheme { scheme, .. } => *scheme != LayoutScheme::Unknown,
        _ => true,
    }
}

impl PeerManager {
    /// Whether `msg` is a forwarded request (see [`Federation`]) for a
    /// session this hub proxies. A create that names a peer always is: the
    /// peer, not this hub, is to create it.
    pub fn is_forwarded(&self, msg: &mut ClientMessage) -> bool {
        match msg.federation() {
            Federation::Forward {
                target: Target::Session(word),
                ..
            } => self.is_federated_session(word),
            Federation::Forward {
                target: Target::Pane(pane),
                ..
            } => self.is_federated_pane(pane),
            Federation::Forward {
                target: Target::Peer(_),
                ..
            } => true,
            Federation::Aggregate | Federation::Hub => false,
        }
    }

    /// Forward the request `msg` from `from` to the peer whose session it
    /// names, under the peer's ids and an id of the hub's own; the answer is
    /// routed back to `from` as it comes. Refused when the peer is unknown or
    /// unreachable, the session or pane is not (or no longer) one it proxies,
    /// or — for input — another viewer holds the hub's input lock.
    pub fn forward(&self, from: Requester, mut msg: ClientMessage) -> Result<(), Refusal> {
        if !sendable(&mut msg) {
            return Ok(());
        }
        let input = writes_input(&msg);
        let Federation::Forward { target, request_id } = msg.federation() else {
            return Err(Refusal {
                request_id: None,
                code: ErrorCode::InternalError,
                message: "not a request a hub forwards".to_string(),
            });
        };
        let client_rid = request_id.as_deref().copied();
        let refuse = |code, message: String| Refusal {
            request_id: client_rid,
            code,
            message,
        };
        let (conn, local_word, missing) = match &target {
            Target::Session(word) => (
                self.conn_for_word(word),
                Some((*word).clone()),
                (
                    ErrorCode::SessionNotFound,
                    format!("session not found: {word}"),
                ),
            ),
            Target::Pane(pane) => {
                let word = parse_pane_id(pane).map(|(word, _)| word.to_string());
                (
                    word.as_deref().and_then(|w| self.conn_for_word(w)),
                    word,
                    (ErrorCode::PaneNotFound, format!("pane not found: {pane}")),
                )
            }
            Target::Peer(peer) => (
                peer.as_deref()
                    .and_then(|peer| self.peers.lock().unwrap().get(peer).cloned()),
                None,
                (
                    ErrorCode::InternalError,
                    format!("peer {} is not connected", peer.as_deref().unwrap_or("")),
                ),
            ),
        };
        let Some(conn) = conn else {
            let (code, message) = missing;
            return Err(refuse(code, message));
        };
        let mut guard = lock(&conn);
        if guard.dead {
            return Err(refuse(
                ErrorCode::InternalError,
                "the peer is unreachable; its link is being re-opened".to_string(),
            ));
        }
        if let Err((code, message)) = retarget(&guard, target, input.then_some(&from)) {
            return Err(refuse(code, message));
        }
        guard.barrier(&from);
        let routed = match (request_id, client_rid) {
            (Some(rid), Some(client_rid)) => {
                *rid = guard.routes.route(Route {
                    from,
                    request_id: client_rid,
                    local_word,
                });
                Some(*rid)
            }
            _ => None,
        };
        if guard.client_tx.send(msg).is_err() {
            // Refused here, so the link's end must not answer it again.
            if let Some(rid) = routed {
                guard.routes.take(rid);
            }
            return Err(refuse(
                ErrorCode::InternalError,
                "the peer's link closed".to_string(),
            ));
        }
        Ok(())
    }

    /// `RequestInputLock` on the proxied pane `local_pane` from `from`: denied
    /// here when another of this hub's clients holds its lock, else asked of
    /// the peer, whose answer is routed back and, when granted, makes `from`
    /// the holder here.
    pub fn request_input_lock(&self, from: &Requester, local_pane: &str) -> Result<(), Refusal> {
        let conn = self.pane_conn(local_pane)?;
        let mut guard = lock(&conn);
        if let Some(holder) = guard.input_locks.get(local_pane)
            && let Some(holder_id) = holder.client_id()
            && Some(holder_id) != from.client_id()
        {
            from.answer(ServerMessage::InputLockDenied {
                pane_id: local_pane.to_string(),
                holder: holder_id,
            });
            return Ok(());
        }
        let remote_pane = upstream_pane(&guard, local_pane)?;
        guard.send(
            from,
            ClientMessage::RequestInputLock {
                pane_id: remote_pane,
            },
        );
        Ok(())
    }

    /// `ReleaseInputLock` on the proxied pane `local_pane` from `from`:
    /// passed to the peer when `from` holds the lock, whose answer releases it
    /// here too; answered with nothing otherwise, as a local release is.
    pub fn release_input_lock(&self, from: &Requester, local_pane: &str) -> Result<(), Refusal> {
        let conn = self.pane_conn(local_pane)?;
        let mut guard = lock(&conn);
        let holds = guard
            .input_locks
            .get(local_pane)
            .is_some_and(|holder| holder.client_id() == from.client_id());
        if holds {
            let remote_pane = upstream_pane(&guard, local_pane)?;
            guard.send(
                from,
                ClientMessage::ReleaseInputLock {
                    pane_id: remote_pane,
                },
            );
        }
        Ok(())
    }

    /// The link of the proxied pane `local_pane`, refused when there is none.
    fn pane_conn(&self, local_pane: &str) -> Result<Arc<Mutex<PeerConnection>>, Refusal> {
        parse_pane_id(local_pane)
            .and_then(|(word, _)| self.conn_for_word(word))
            .ok_or_else(|| Refusal {
                request_id: None,
                code: ErrorCode::PaneNotFound,
                message: format!("pane not found: {local_pane}"),
            })
    }

    /// Register the session the peer created (or restored) for a forwarded
    /// `SessionCreate` under a fresh local word, returning its local entry.
    /// A refresh of the peer's list may have registered it already; then it
    /// keeps that word. `None` when the peer was closed meanwhile or the word
    /// pool is empty. Under the membership gate, so a concurrent re-list
    /// cannot register it under a second word.
    fn register_created(
        &self,
        app: &ServerApp,
        conn: &Arc<Mutex<PeerConnection>>,
        peer_id: &str,
        remote_entry: SessionEntry,
    ) -> Option<SessionEntry> {
        let remote_word = remote_entry.meta.word_id.clone();
        let _membership = self.membership();
        if !self.is_open(peer_id, conn) {
            return None;
        }
        let registered = {
            let guard = lock(conn);
            guard
                .remote_to_local
                .get(&remote_word)
                .and_then(|local| guard.sessions.get(local))
                .cloned()
        };
        if registered.is_some() {
            return registered;
        }
        let local_word = app.draw_word()?;
        let entry = {
            let mut guard = lock(conn);
            guard.register_session(local_word.clone(), remote_word, remote_entry, peer_id);
            guard.sessions.get(&local_word).cloned()
        };
        self.word_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(local_word, peer_id.to_string());
        entry
    }
}

/// Rewrite `target` to the peer's ids, or say why it cannot be: the session
/// or pane is no longer one the link proxies, or — for input from `writer` —
/// another client holds the hub's input lock on the pane.
fn retarget(
    guard: &PeerConnection,
    target: Target<'_>,
    writer: Option<&Requester>,
) -> Result<(), (ErrorCode, String)> {
    match target {
        Target::Session(word) => {
            let Some(remote) = guard.local_to_remote.get(word.as_str()).cloned() else {
                return Err((
                    ErrorCode::SessionNotFound,
                    format!("session not found: {word}"),
                ));
            };
            *word = remote;
        }
        Target::Pane(pane) => {
            if let Some(writer) = writer
                && let Some(holder) = guard.input_locks.get(pane.as_str())
                && holder.client_id() != writer.client_id()
            {
                return Err((
                    ErrorCode::InputLocked,
                    format!("pane {pane} is locked by another client"),
                ));
            }
            let Some(remote) = guard.to_remote_pane(pane) else {
                return Err((ErrorCode::PaneNotFound, format!("pane not found: {pane}")));
            };
            *pane = remote;
        }
        Target::Peer(peer) => *peer = None,
    }
    Ok(())
}

/// The peer's id for the proxied pane `local_pane`, refused when its session
/// is gone.
fn upstream_pane(guard: &PeerConnection, local_pane: &str) -> Result<String, Refusal> {
    guard.to_remote_pane(local_pane).ok_or_else(|| Refusal {
        request_id: None,
        code: ErrorCode::PaneNotFound,
        message: format!("pane not found: {local_pane}"),
    })
}

/// Answer the forwarded request `msg` answers, if one is waiting — `None`
/// then — or hand `msg` back. The answer goes to the sender alone, under its
/// own request id and ids. A `SessionCreated` registers the new session
/// here first; a `SessionClosed` closes it here, for every client, after the
/// sender has its answer (a local close answers, then broadcasts).
pub(super) fn answer_routed(
    app: &Weak<ServerApp>,
    conn: &Arc<Mutex<PeerConnection>>,
    peer_id: &PeerId,
    mut msg: ServerMessage,
) -> Option<ServerMessage> {
    let Some(route) = reply_request_id(&msg).and_then(|id| lock(conn).routes.take(id)) else {
        return Some(msg);
    };
    let closed = match &msg {
        ServerMessage::SessionClosed { word_id, .. } => Some(word_id.clone()),
        _ => None,
    };
    if let ServerMessage::SessionCreated { entry, .. } = &mut msg {
        let local = app.upgrade().and_then(|app| {
            app.peer_manager
                .register_created(&app, conn, peer_id, entry.clone())
        });
        match local {
            Some(local) => *entry = local,
            None => {
                msg = ServerMessage::Error {
                    request_id: None,
                    code: ErrorCode::SessionLimitReached,
                    message: "the peer created the session, but this hub has no word left for it"
                        .to_string(),
                };
            }
        }
    } else if let Some(local_word) = &route.local_word {
        localize_reply(&mut msg, local_word);
    }
    set_reply_request_id(&mut msg, route.request_id);
    route.from.answer(msg);
    if let Some(remote_word) = closed {
        super::feed::on_remote_session_closed(app, conn, &remote_word);
    }
    None
}

/// Answer the sender of the request `msg` answers when it is one of the
/// answers that carry no `request_id` — an `Error`, `SessionRenamed`, or an
/// input-lock reply — or hand `msg` back. It is the oldest run's (see
/// [`super::routes`]). A lock granted or released moves the hub's lock with
/// it; the hub's own requests' answers go nowhere.
pub(super) fn answer_unrequested(
    conn: &Mutex<PeerConnection>,
    mut msg: ServerMessage,
) -> Option<ServerMessage> {
    let unrequested = matches!(
        msg,
        ServerMessage::Error {
            request_id: None,
            ..
        } | ServerMessage::SessionRenamed { .. }
            | ServerMessage::InputLockGranted { .. }
            | ServerMessage::InputLockDenied { .. }
            | ServerMessage::InputLockReleased { .. }
    );
    if !unrequested {
        return Some(msg);
    }
    let mut guard = lock(conn);
    let Some(from) = guard.routes.front().cloned() else {
        debug!(?msg, "an answer from the peer with no request waiting");
        return None;
    };
    if let Some(local_word) = reply_word(&msg).and_then(|w| guard.remote_to_local.get(w).cloned()) {
        localize_reply(&mut msg, &local_word);
    }
    let msg = guard.on_lock_reply(&from, msg);
    drop(guard);
    from.answer(msg);
    None
}

/// The link dropped: answer every request still waiting with an `Error`,
/// since its answer will never come, and tell every lock holder its lock is
/// gone (a new link holds none).
pub(super) fn fail_in_flight(conn: &mut PeerConnection) {
    for route in conn.routes.drain() {
        route.from.answer(ServerMessage::Error {
            request_id: Some(route.request_id),
            code: ErrorCode::InternalError,
            message: "the peer became unreachable before it answered".to_string(),
        });
    }
    for (pane_id, holder) in conn.input_locks.drain() {
        holder.answer(ServerMessage::InputLockReleased { pane_id });
    }
}

#[cfg(test)]
mod tests {
    //! A hub proxying `fedlocal` from a peer played over channels
    //! (`install_channel_peer`, on the paused clock): what goes up the link,
    //! and what each client is answered.

    use kmux_protocol::messages::{ClientId, LayoutNode, RequestId, TabInfo, TermSize};
    use tokio::sync::mpsc;

    use super::*;
    use crate::federation::link::testing::{WAIT, next_upstream};
    use crate::fixtures::{fixture_app, make_outbound};
    use crate::outbound::OutboundRx;

    /// The hub, what it sends the peer, and the sender that plays the peer.
    struct Hub {
        app: Arc<ServerApp>,
        upstream: mpsc::UnboundedReceiver<ClientMessage>,
        peer: mpsc::UnboundedSender<ServerMessage>,
    }

    fn fixture_hub() -> Hub {
        let app = Arc::new(fixture_app());
        let (upstream, peer) = app.install_channel_peer("fedlocal", "fedremote");
        Hub {
            app,
            upstream,
            peer,
        }
    }

    /// A client of the hub, and its control lane.
    fn client(id: u64) -> (Requester, OutboundRx) {
        let (ctrl, rx) = make_outbound();
        (Requester::client(ClientId(id), ctrl), rx)
    }

    /// Play the peer for everything the hub has sent it so far, in order, as
    /// a daemon answers one connection: each ping answered, each lock request
    /// granted, each release confirmed.
    fn play_peer(hub: &mut Hub) {
        while let Ok(msg) = hub.upstream.try_recv() {
            let reply = match msg {
                ClientMessage::Ping { seq } => ServerMessage::Pong { seq },
                ClientMessage::RequestInputLock { pane_id } => {
                    ServerMessage::InputLockGranted { pane_id }
                }
                ClientMessage::ReleaseInputLock { pane_id } => {
                    ServerMessage::InputLockReleased { pane_id }
                }
                _ => continue,
            };
            hub.peer.send(reply).unwrap();
        }
    }

    /// A client of the hub viewing `fedlocal/0`, its control lane, and its
    /// pane stream; the peer has played what its attach sent.
    fn viewing_client(
        hub: &mut Hub,
        id: u64,
    ) -> (Requester, OutboundRx, mpsc::Receiver<ServerMessage>) {
        let (ctrl, rx) = make_outbound();
        let (data_tx, data_rx) = mpsc::channel(8);
        assert!(hub.app.federated_attach(
            "fedlocal/0",
            ClientId(id),
            data_tx,
            ctrl.clone(),
            TermSize::default(),
        ));
        play_peer(hub);
        (Requester::client(ClientId(id), ctrl), rx, data_rx)
    }

    /// The next thing the hub sends the peer that is not a ping.
    async fn next_request(upstream: &mut mpsc::UnboundedReceiver<ClientMessage>) -> ClientMessage {
        next_upstream(upstream, |m| {
            (!matches!(m, ClientMessage::Ping { .. })).then(|| m.clone())
        })
        .await
    }

    /// The next thing a client is answered.
    async fn answer(rx: &mut OutboundRx) -> ServerMessage {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("an answer within the bound")
            .expect("the lane is open")
    }

    /// The hub's id for a request it sent up.
    fn hub_id(msg: &mut ClientMessage) -> RequestId {
        match msg.federation() {
            Federation::Forward {
                request_id: Some(id),
                ..
            } => *id,
            _ => panic!("a forwarded request with an id: {msg:?}"),
        }
    }

    /// A pane as a peer describes it, attached to one of the peer's clients.
    fn sample_pane(pane_id: &str) -> kmux_protocol::messages::PaneInfo {
        kmux_protocol::messages::PaneInfo {
            pane_id: pane_id.to_string(),
            pane_index: 1,
            program: "sh".to_string(),
            size: TermSize::default(),
            attached_clients: vec![ClientId(7)],
            status: kmux_protocol::messages::SessionStatus::Running,
            title: String::new(),
            progress_state: Default::default(),
            progress: None,
        }
    }

    fn tab() -> TabInfo {
        TabInfo {
            tab_index: 1,
            name: "2".to_string(),
            layout: LayoutNode::single(1),
            focused_pane: 1,
        }
    }

    /// A request, the peer's answer to it (under a placeholder id), and how
    /// the sender must see that answer begin.
    type Row = (ClientMessage, ServerMessage, &'static str);

    /// Forward each row's request as one client, answer it as the peer, and
    /// check the request went up under the peer's ids and an id of the hub's
    /// own, and the answer came back under the sender's.
    async fn round_trip(rows: Vec<Row>) {
        let mut hub = fixture_hub();
        let (from, mut answers) = client(1);
        for (request, mut reply, expected) in rows {
            let name = format!("{request:?}");
            hub.app
                .peer_manager
                .forward(from.clone(), request)
                .expect("forwarded");
            let mut sent = next_request(&mut hub.upstream).await;
            let text = format!("{sent:?}");
            assert!(
                text.contains("fedremote") && !text.contains("fedlocal"),
                "{name} goes up under the peer's ids: {text}"
            );
            assert!(
                !text.contains("Unknown"),
                "the hub never sends Unknown: {text}"
            );
            let id = hub_id(&mut sent);
            assert!(id >= 2, "the hub's own id space: {id}");
            set_reply_request_id(&mut reply, id);
            hub.peer.send(reply).unwrap();
            let got = format!("{:?}", answer(&mut answers).await);
            assert!(got.starts_with(expected), "{name}: {got}");
        }
    }

    /// Answers naming a pane come back under the sender's pane id (issue
    /// #227).
    #[tokio::test(start_paused = true)]
    async fn an_answer_naming_a_pane_comes_back_under_the_senders_ids() {
        let size = TermSize::default();
        round_trip(vec![
        (
            ClientMessage::PaneCreate {
                request_id: 10,
                word_id: "fedlocal".into(),
                program: None,
                args: vec![],
                size,
            },
            ServerMessage::PaneCreated {
                request_id: 0,
                pane_id: "fedremote/1".into(),
                session_word_id: "fedremote".into(),
                size,
            },
            "PaneCreated { request_id: 10, pane_id: \"fedlocal/1\", session_word_id: \"fedlocal\"",
        ),
        (
            ClientMessage::PaneClose {
                request_id: 12,
                pane_id: "fedlocal/1".into(),
            },
            ServerMessage::PaneClosed {
                request_id: 0,
                pane_id: "fedremote/1".into(),
                exit_code: Some(0),
            },
            "PaneClosed { request_id: 12, pane_id: \"fedlocal/1\"",
        ),
        (
            ClientMessage::FetchHistory {
                request_id: 14,
                pane_id: "fedlocal/0".into(),
                start_index: 0,
                count: 5,
            },
            ServerMessage::HistoryLines {
                request_id: 0,
                pane_id: "fedremote/0".into(),
                first_index: 0,
                lines: vec![],
                history_total: 0,
                sent_at_ms: 0,
            },
            "HistoryLines { request_id: 14, pane_id: \"fedlocal/0\"",
        ),
        (
            ClientMessage::Notify {
                request_id: 17,
                pane_id: "fedlocal/0".into(),
                kind: kmux_protocol::messages::AttentionKind::Unknown,
                title: String::new(),
                body: String::new(),
            },
            ServerMessage::NotifyAccepted { request_id: 0 },
            "NotifyAccepted { request_id: 17 }",
        ),
        ])
        .await;
    }

    /// Answers naming a session come back under the sender's word, and an
    /// error under the sender's id, with the peer's code (issue #227).
    #[tokio::test(start_paused = true)]
    async fn an_answer_naming_a_session_comes_back_under_the_senders_ids() {
        let size = TermSize::default();
        round_trip(vec![
            (
                ClientMessage::TabCreate {
                    request_id: 11,
                    word_id: "fedlocal".into(),
                    program: None,
                    args: vec![],
                    size,
                },
                ServerMessage::TabCreated {
                    request_id: 0,
                    word_id: "fedremote".into(),
                    tab: tab(),
                },
                "TabCreated { request_id: 11, word_id: \"fedlocal\"",
            ),
            (
                ClientMessage::TabClose {
                    request_id: 13,
                    word_id: "fedlocal".into(),
                    tab_index: 1,
                },
                ServerMessage::TabClosed {
                    request_id: 0,
                    word_id: "fedremote".into(),
                    tab_index: 1,
                },
                "TabClosed { request_id: 13, word_id: \"fedlocal\"",
            ),
            (
                ClientMessage::PaneSplit {
                    request_id: 19,
                    word_id: "fedlocal".into(),
                    tab_index: 0,
                    from_pane: 0,
                    dir: kmux_protocol::messages::SplitDir::Vertical,
                    program: None,
                    args: vec![],
                    size,
                },
                ServerMessage::PaneSplit {
                    request_id: 0,
                    word_id: "fedremote".into(),
                    tab_index: 0,
                    new_pane: sample_pane("fedremote/1"),
                    layout: LayoutNode::single(1),
                },
                "PaneSplit { request_id: 19, word_id: \"fedlocal\", tab_index: 0, new_pane: PaneInfo { pane_id: \"fedlocal/1\"",
            ),
            (
                ClientMessage::ClientList {
                    request_id: 15,
                    word_id: "fedlocal".into(),
                },
                ServerMessage::ClientListResult {
                    request_id: 0,
                    word_id: "fedremote".into(),
                    clients: vec![],
                },
                "ClientListResult { request_id: 15, word_id: \"fedlocal\"",
            ),
            (
                ClientMessage::KickClient {
                    request_id: 16,
                    word_id: "fedlocal".into(),
                    client_id: ClientId(9),
                },
                ServerMessage::ClientKicked {
                    request_id: 0,
                    word_id: "fedremote".into(),
                    client_id: ClientId(9),
                },
                "ClientKicked { request_id: 16, word_id: \"fedlocal\"",
            ),
            (
                ClientMessage::SessionRename {
                    request_id: 18,
                    word_id: "fedlocal".into(),
                    new_name: "n".into(),
                },
                ServerMessage::Error {
                    request_id: Some(0),
                    code: ErrorCode::SessionNotFound,
                    message: "gone".into(),
                },
                "Error { request_id: Some(18), code: SessionNotFound",
            ),
        ])
        .await;
    }

    /// Answers without a `request_id` go to the client whose message caused
    /// them: a change of sender is closed with a ping, and the peer's pong
    /// moves the answers on to the next sender (issue #227).
    #[tokio::test(start_paused = true)]
    async fn an_answer_without_an_id_reaches_the_client_that_asked() {
        let mut hub = fixture_hub();
        let (a, mut a_answers) = client(1);
        let (b, mut b_answers) = client(2);
        let input = |data: &[u8]| ClientMessage::PtyInput {
            pane_id: "fedlocal/0".into(),
            data: data.to_vec(),
        };
        let mgr = &hub.app.peer_manager;
        mgr.forward(a.clone(), input(b"a")).unwrap();
        mgr.forward(
            b.clone(),
            ClientMessage::SessionRename {
                request_id: 3,
                word_id: "fedlocal".into(),
                new_name: "work".into(),
            },
        )
        .unwrap();
        let barrier = next_upstream(&mut hub.upstream, |m| match m {
            ClientMessage::Ping { seq } => Some(*seq),
            _ => None,
        })
        .await;
        let error = |message: &str| ServerMessage::Error {
            request_id: None,
            code: ErrorCode::PaneNotFound,
            message: message.to_string(),
        };
        hub.peer.send(error("a's")).unwrap();
        hub.peer.send(ServerMessage::Pong { seq: barrier }).unwrap();
        hub.peer
            .send(ServerMessage::SessionRenamed {
                word_id: "fedremote".into(),
                new_name: "work".into(),
            })
            .unwrap();
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::Error { message, .. } if message == "a's"
        ));
        assert!(matches!(
            answer(&mut b_answers).await,
            ServerMessage::SessionRenamed { word_id, .. } if word_id == "fedlocal"
        ));
        assert!(a_answers.try_recv().is_err(), "a is answered once");
    }

    /// What cannot be forwarded is refused at once, under the request's own
    /// id and with the code a local daemon would give, and nothing goes up.
    #[tokio::test(start_paused = true)]
    async fn a_request_that_cannot_be_forwarded_is_refused_at_once() {
        let mut hub = fixture_hub();
        let (from, mut answers) = client(1);
        let refused = |msg| hub.app.peer_manager.forward(from.clone(), msg).unwrap_err();
        let create = ClientMessage::SessionCreate {
            request_id: 1,
            name: None,
            cwd: None,
            program: None,
            args: vec![],
            size: TermSize::default(),
            peer: Some("nowhere".into()),
        };
        let cases = [
            (create, Some(1), ErrorCode::InternalError),
            (
                ClientMessage::SessionClose {
                    request_id: 2,
                    word_id: "nosuch".into(),
                },
                Some(2),
                ErrorCode::SessionNotFound,
            ),
            (
                ClientMessage::Signal {
                    pane_id: "nosuch/0".into(),
                    signal: 2,
                },
                None,
                ErrorCode::PaneNotFound,
            ),
            (ClientMessage::ChannelReady, None, ErrorCode::InternalError),
        ];
        for (msg, request_id, code) in cases {
            let refusal = refused(msg);
            assert_eq!((refusal.request_id, refusal.code), (request_id, code));
        }
        // A scheme this hub does not know is ignored, as a local daemon
        // ignores it: not refused, not sent.
        hub.app
            .peer_manager
            .forward(
                from.clone(),
                ClientMessage::ApplyLayoutScheme {
                    word_id: "fedlocal".into(),
                    tab_index: 0,
                    scheme: LayoutScheme::Unknown,
                },
            )
            .expect("ignored");
        assert!(hub.upstream.try_recv().is_err(), "nothing went up");

        // A request the link will not take is refused here, and its route
        // with it: the link's end must not answer it a second time.
        drop(hub.upstream);
        let refusal = refused(ClientMessage::SessionRename {
            request_id: 3,
            word_id: "fedlocal".into(),
            new_name: "n".into(),
        });
        assert_eq!(
            (refusal.request_id, refusal.code),
            (Some(3), ErrorCode::InternalError)
        );

        // An unreachable peer refuses everything.
        let conn = Arc::clone(&hub.app.peer_manager.peers.lock().unwrap()["peer:1"]);
        crate::federation::link::link_down(&conn);
        let refusal = refused(ClientMessage::SessionRename {
            request_id: 4,
            word_id: "fedlocal".into(),
            new_name: "n".into(),
        });
        assert_eq!(
            (refusal.request_id, refusal.code),
            (Some(4), ErrorCode::InternalError)
        );
        assert!(answers.try_recv().is_err(), "nothing is answered twice");
    }

    /// Which messages the hub forwards: those naming a session it proxies,
    /// and a create naming a peer; not the aggregated or the hub's own.
    #[tokio::test(start_paused = true)]
    async fn only_a_request_for_a_proxied_session_is_forwarded() {
        let hub = fixture_hub();
        let forwarded = |mut msg: ClientMessage| hub.app.peer_manager.is_forwarded(&mut msg);
        assert!(forwarded(ClientMessage::SessionRename {
            request_id: 1,
            word_id: "fedlocal".into(),
            new_name: "n".into(),
        }));
        assert!(!forwarded(ClientMessage::SessionRename {
            request_id: 1,
            word_id: "hosted".into(),
            new_name: "n".into(),
        }));
        assert!(forwarded(ClientMessage::Signal {
            pane_id: "fedlocal/0".into(),
            signal: 2,
        }));
        assert!(!forwarded(ClientMessage::Signal {
            pane_id: "hosted/0".into(),
            signal: 2,
        }));
        assert!(forwarded(ClientMessage::SessionCreate {
            request_id: 1,
            name: None,
            cwd: None,
            program: None,
            args: vec![],
            size: TermSize::default(),
            peer: Some("anywhere".into()),
        }));
        assert!(!forwarded(ClientMessage::Detach {
            pane_id: "fedlocal/0".into(),
        }));
        assert!(!forwarded(ClientMessage::SessionList { request_id: 1 }));
    }

    /// A create on a peer is registered under a fresh local word, and its
    /// requester is answered with that word (issue #121, now on the one
    /// path).
    #[tokio::test(start_paused = true)]
    async fn a_session_created_on_the_peer_is_registered_under_a_fresh_word() {
        let mut hub = fixture_hub();
        let (from, mut answers) = client(1);
        hub.app
            .peer_manager
            .forward(
                from,
                ClientMessage::SessionCreate {
                    request_id: 21,
                    name: Some("work".into()),
                    cwd: None,
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                    peer: Some("peer:1".into()),
                },
            )
            .unwrap();
        let sent = next_request(&mut hub.upstream).await;
        let ClientMessage::SessionCreate {
            request_id: id,
            peer: None,
            ..
        } = sent
        else {
            panic!("the peer creates it on itself: {sent:?}");
        };
        hub.peer
            .send(ServerMessage::SessionCreated {
                request_id: id,
                entry: crate::federation::sample_remote_entry("newremote"),
            })
            .unwrap();
        let ServerMessage::SessionCreated { request_id, entry } = answer(&mut answers).await else {
            panic!("a SessionCreated");
        };
        assert_eq!(request_id, 21);
        let word = entry.meta.word_id;
        assert_ne!(word, "newremote", "a word of the hub's own");
        assert!(hub.app.is_federated_session(&word));
        assert_eq!(entry.peer.as_deref(), Some("peer:1"));
    }

    /// The hub arbitrates the input lock between its own clients and holds
    /// the peer's for the one holding its own; the others' input is refused
    /// (issue #227).
    #[tokio::test(start_paused = true)]
    async fn the_hub_arbitrates_the_input_lock_between_its_clients() {
        let mut hub = fixture_hub();
        let (a, mut a_answers, _a_pane) = viewing_client(&mut hub, 1);
        let (b, mut b_answers, _b_pane) = viewing_client(&mut hub, 2);
        let mgr = &hub.app.peer_manager;
        let pane = "fedlocal/0";

        mgr.request_input_lock(&a, pane).unwrap();
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::RequestInputLock { pane_id } if pane_id == "fedremote/0"
        ));
        hub.peer
            .send(ServerMessage::InputLockGranted {
                pane_id: "fedremote/0".into(),
            })
            .unwrap();
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockGranted { pane_id } if pane_id == pane
        ));

        // B is denied here, naming A, without asking the peer.
        mgr.request_input_lock(&b, pane).unwrap();
        assert!(matches!(
            answer(&mut b_answers).await,
            ServerMessage::InputLockDenied {
                holder: ClientId(1),
                ..
            }
        ));
        let input = |from: &Requester| {
            mgr.forward(
                from.clone(),
                ClientMessage::PtyInput {
                    pane_id: pane.into(),
                    data: b"x".to_vec(),
                },
            )
        };
        assert_eq!(input(&b).unwrap_err().code, ErrorCode::InputLocked);
        input(&a).expect("the holder types");
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::PtyInput { .. }
        ));
        // Only input is locked: B still reads the pane's history.
        mgr.forward(
            b.clone(),
            ClientMessage::FetchHistory {
                request_id: 9,
                pane_id: pane.into(),
                start_index: 0,
                count: 1,
            },
        )
        .expect("B reads under A's lock");
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::FetchHistory { .. }
        ));

        // Only the holder's release goes up; the peer's answer frees it here.
        mgr.release_input_lock(&b, pane).unwrap();
        mgr.release_input_lock(&a, pane).unwrap();
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::ReleaseInputLock { pane_id } if pane_id == "fedremote/0"
        ));
        hub.peer
            .send(ServerMessage::InputLockReleased {
                pane_id: "fedremote/0".into(),
            })
            .unwrap();
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockReleased { .. }
        ));
        input(&b).expect("unlocked again");
        assert!(b_answers.try_recv().is_err());
        assert_eq!(
            mgr.request_input_lock(&a, "nosuch/0").unwrap_err().code,
            ErrorCode::PaneNotFound
        );
    }

    /// Two requests made while the lock was free are both granted by the
    /// peer, which sees one client; the hub gives it to the first and denies
    /// the second. A grant for a client that stopped viewing the pane while
    /// it asked is given back to the peer at once. And a closed session's
    /// lock goes with it (issue #227).
    #[tokio::test(start_paused = true)]
    async fn the_hub_keeps_one_holder_whatever_the_peer_grants() {
        let mut hub = fixture_hub();
        let (a, mut a_answers, _a_pane) = viewing_client(&mut hub, 1);
        let (b, mut b_answers, _b_pane) = viewing_client(&mut hub, 2);
        let pane = "fedlocal/0";
        hub.app.peer_manager.request_input_lock(&a, pane).unwrap();
        hub.app.peer_manager.request_input_lock(&b, pane).unwrap();
        play_peer(&mut hub);
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockGranted { .. }
        ));
        assert!(matches!(
            answer(&mut b_answers).await,
            ServerMessage::InputLockDenied {
                holder: ClientId(1),
                ..
            }
        ));

        // C asks, then stops viewing before the grant comes back.
        let (c, mut c_answers, _c_pane) = viewing_client(&mut hub, 3);
        let mgr = &hub.app.peer_manager;
        mgr.release_input_lock(&a, pane).unwrap();
        play_peer(&mut hub);
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockReleased { .. }
        ));
        let mgr = &hub.app.peer_manager;
        mgr.request_input_lock(&c, pane).unwrap();
        mgr.detach_viewer(pane, ClientId(3));
        play_peer(&mut hub);
        assert!(matches!(
            answer(&mut c_answers).await,
            ServerMessage::InputLockGranted { .. }
        ));
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::ReleaseInputLock { pane_id } if pane_id == "fedremote/0"
        ));
        let conn = Arc::clone(&hub.app.peer_manager.peers.lock().unwrap()["peer:1"]);
        assert!(lock(&conn).input_locks.is_empty(), "nobody holds it");

        // A closed session's lock goes with it.
        lock(&conn).input_locks.insert(pane.to_string(), a.clone());
        hub.peer
            .send(ServerMessage::Event {
                event: kmux_protocol::messages::SessionEventMsg::SessionClosed {
                    word_id: "fedremote".into(),
                },
            })
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(
            lock(&conn).input_locks.is_empty(),
            "closed with its session"
        );
    }

    /// A holder asking again and refused by the peer (a direct client of the
    /// peer took the lock) no longer holds it here either.
    #[tokio::test(start_paused = true)]
    async fn a_holder_the_peer_refuses_no_longer_holds_the_lock() {
        let mut hub = fixture_hub();
        let (a, mut a_answers, _a_pane) = viewing_client(&mut hub, 1);
        let (b, _b_answers, _b_pane) = viewing_client(&mut hub, 2);
        let pane = "fedlocal/0";
        hub.app.peer_manager.request_input_lock(&a, pane).unwrap();
        play_peer(&mut hub);
        answer(&mut a_answers).await;
        hub.app.peer_manager.request_input_lock(&a, pane).unwrap();
        while let Ok(msg) = hub.upstream.try_recv() {
            if let ClientMessage::Ping { seq } = msg {
                hub.peer.send(ServerMessage::Pong { seq }).unwrap();
            }
        }
        hub.peer
            .send(ServerMessage::InputLockDenied {
                pane_id: "fedremote/0".into(),
                holder: ClientId(9),
            })
            .unwrap();
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockDenied { .. }
        ));
        hub.app
            .peer_manager
            .forward(
                b,
                ClientMessage::PtyInput {
                    pane_id: pane.into(),
                    data: b"y".to_vec(),
                },
            )
            .expect("the hub no longer holds it for A");
    }

    /// A grant given back for a client that stopped viewing can reach the
    /// peer after it granted the lock again for the client holding it here:
    /// the peer's confirmation makes the hub ask for the lock again, and a
    /// refusal tells the holder its lock is gone. Meanwhile the holder keeps
    /// it here (issue #227).
    #[tokio::test(start_paused = true)]
    async fn giving_back_a_stale_grant_asks_again_for_the_holder() {
        let mut hub = fixture_hub();
        let (a, mut a_answers, _a_pane) = viewing_client(&mut hub, 1);
        let (b, _b_answers, _b_pane) = viewing_client(&mut hub, 2);
        let (c, _c_answers, _c_pane) = viewing_client(&mut hub, 3);
        let pane = "fedlocal/0";
        let mgr = &hub.app.peer_manager;
        mgr.request_input_lock(&c, pane).unwrap();
        mgr.detach_viewer(pane, ClientId(3));
        mgr.request_input_lock(&a, pane).unwrap();
        play_peer(&mut hub);
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockGranted { .. }
        ));
        // The give-back of C's grant, and the peer's confirmation of it.
        play_peer(&mut hub);
        assert!(matches!(
            next_request(&mut hub.upstream).await,
            ClientMessage::RequestInputLock { pane_id } if pane_id == "fedremote/0"
        ));
        let input = |from: &Requester| {
            hub.app.peer_manager.forward(
                from.clone(),
                ClientMessage::PtyInput {
                    pane_id: pane.into(),
                    data: b"x".to_vec(),
                },
            )
        };
        assert_eq!(input(&b).unwrap_err().code, ErrorCode::InputLocked);

        // A direct client of the peer took the lock in between.
        hub.peer
            .send(ServerMessage::InputLockDenied {
                pane_id: "fedremote/0".into(),
                holder: ClientId(9),
            })
            .unwrap();
        assert!(matches!(
            answer(&mut a_answers).await,
            ServerMessage::InputLockReleased { pane_id } if pane_id == pane
        ));
        input(&b).expect("nobody holds it here any more");
    }

    /// A client that resumes on a new channel and re-attaches keeps its
    /// lock when the old channel ends, as a local daemon keeps it (#208).
    #[tokio::test(start_paused = true)]
    async fn a_resumed_holder_keeps_its_lock_when_the_old_channel_ends() {
        let mut hub = fixture_hub();
        let (a, _a_answers, _a_pane) = viewing_client(&mut hub, 1);
        let (b, _b_answers, _b_pane) = viewing_client(&mut hub, 2);
        let pane = "fedlocal/0";
        hub.app.peer_manager.request_input_lock(&a, pane).unwrap();
        play_peer(&mut hub);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let (resumed, _resumed_answers, _resumed_pane) = viewing_client(&mut hub, 1);
        if let Requester::Client { ctrl: old, .. } = &a {
            hub.app.peer_manager.detach_channel(ClientId(1), old);
        }
        assert!(hub.upstream.try_recv().is_err(), "nothing is given back");
        let input = |from: &Requester| {
            hub.app.peer_manager.forward(
                from.clone(),
                ClientMessage::PtyInput {
                    pane_id: pane.into(),
                    data: b"x".to_vec(),
                },
            )
        };
        assert_eq!(input(&b).unwrap_err().code, ErrorCode::InputLocked);
        input(&resumed).expect("the resumed holder types");
    }

    /// A link that drops answers everything still waiting with an error, and
    /// tells a lock holder its lock is gone: the answers will never come.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_link_answers_what_was_in_flight() {
        let mut hub = fixture_hub();
        let (a, mut answers, _pane) = viewing_client(&mut hub, 1);
        let mgr = &hub.app.peer_manager;
        mgr.request_input_lock(&a, "fedlocal/0").unwrap();
        next_request(&mut hub.upstream).await;
        hub.peer
            .send(ServerMessage::InputLockGranted {
                pane_id: "fedremote/0".into(),
            })
            .unwrap();
        answer(&mut answers).await;
        mgr.forward(
            a,
            ClientMessage::FetchHistory {
                request_id: 30,
                pane_id: "fedlocal/0".into(),
                start_index: 0,
                count: 1,
            },
        )
        .unwrap();
        next_request(&mut hub.upstream).await;
        drop(hub.peer);

        let mut got = [answer(&mut answers).await, answer(&mut answers).await];
        got.sort_by_key(|m| format!("{m:?}"));
        assert!(matches!(
            &got[0],
            ServerMessage::Error {
                request_id: Some(30),
                code: ErrorCode::InternalError,
                ..
            }
        ));
        assert!(matches!(
            &got[1],
            ServerMessage::InputLockReleased { pane_id } if pane_id == "fedlocal/0"
        ));
    }
}
