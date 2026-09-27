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
//! the session is gone — is refused at once with a typed [`Refusal`]; one in
//! flight when the link drops is answered by [`fail_in_flight`]. Nothing is
//! dropped unanswered.

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
    /// unreachable, or the session or pane is not (or no longer) one it
    /// proxies.
    pub fn forward(&self, from: Requester, mut msg: ClientMessage) -> Result<(), Refusal> {
        if !sendable(&mut msg) {
            return Ok(());
        }
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
        match target {
            Target::Session(word) => {
                let Some(remote) = guard.local_to_remote.get(word.as_str()).cloned() else {
                    return Err(refuse(
                        ErrorCode::SessionNotFound,
                        format!("session not found: {word}"),
                    ));
                };
                *word = remote;
            }
            Target::Pane(pane) => {
                let Some(remote) = guard.to_remote_pane(pane) else {
                    return Err(refuse(
                        ErrorCode::PaneNotFound,
                        format!("pane not found: {pane}"),
                    ));
                };
                *pane = remote;
            }
            Target::Peer(peer) => *peer = None,
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
/// answers that carry no `request_id` — an `Error` or `SessionRenamed` — or
/// hand `msg` back. It is the oldest run's (see [`super::routes`]); the hub's
/// own requests' answers go nowhere.
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
    );
    if !unrequested {
        return Some(msg);
    }
    let guard = lock(conn);
    let Some(from) = guard.routes.front().cloned() else {
        debug!(?msg, "an answer from the peer with no request waiting");
        return None;
    };
    if let Some(local_word) = reply_word(&msg).and_then(|w| guard.remote_to_local.get(w).cloned()) {
        localize_reply(&mut msg, &local_word);
    }
    drop(guard);
    from.answer(msg);
    None
}

/// The link dropped: answer every request still waiting with an `Error`,
/// since its answer will never come.
pub(super) fn fail_in_flight(conn: &mut PeerConnection) {
    for route in conn.routes.drain() {
        route.from.answer(ServerMessage::Error {
            request_id: Some(route.request_id),
            code: ErrorCode::InternalError,
            message: "the peer became unreachable before it answered".to_string(),
        });
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

    /// A link that drops answers everything still waiting with an error: the
    /// answers will never come.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_link_answers_what_was_in_flight() {
        let mut hub = fixture_hub();
        let (a, mut answers) = client(1);
        hub.app
            .peer_manager
            .forward(
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

        assert!(matches!(
            answer(&mut answers).await,
            ServerMessage::Error {
                request_id: Some(30),
                code: ErrorCode::InternalError,
                ..
            }
        ));
    }
}
