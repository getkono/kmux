//! Federation entry points on [`ServerApp`], compiled unconditionally.
//!
//! The dispatch layer ([`crate::client_handler`]) routes pane and session
//! operations through these thin wrappers without any `#[cfg]` of its own: when
//! the `federation` feature is disabled every wrapper collapses to the
//! local-only / "not supported" behaviour, and when it is enabled they delegate
//! to the [`crate::federation::PeerManager`] held on `ServerApp`.
//!
//! Word IDs for federated sessions are drawn from the **same**
//! [`WordlistSampler`](crate::wordlist::WordlistSampler) as local sessions
//! ([`ServerApp::draw_word`]) so a proxied session can never collide with a
//! locally-hosted one.

use kmux_protocol::messages::{
    ClientId, ClientMessage, ErrorCode, PaneProcesses, PeerId, PeerTarget, RequestId,
    ServerMessage, SessionEntry, TermSize,
};
use tokio::sync::mpsc;

use super::ServerApp;
use crate::outbound::OutboundTx;

/// The `PeerError` reason for an `OpenPeer` whose target is a kind this
/// daemon does not know (`PeerTarget::Unknown`, from a newer client).
pub(crate) const UNSUPPORTED_PEER_TARGET: &str =
    "this daemon does not support that kind of peer target; update kmuxd";

impl ServerApp {
    /// Draw a unique session word from the shared pool, or `None` when exhausted.
    /// Federated sessions use this so their local IDs never collide with
    /// locally-hosted sessions or with another peer's proxied sessions.
    #[cfg(feature = "federation")]
    pub fn draw_word(&self) -> Option<String> {
        let mut wl = self.wordlist.lock().unwrap();
        let mut rng = self.rng.lock().unwrap();
        wl.draw(&mut rng)
    }

    /// How many session words the shared pool holds, for tests to see a
    /// word drawn or returned.
    #[cfg(all(test, feature = "federation"))]
    pub(crate) fn available_words(&self) -> usize {
        self.wordlist
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .available_count()
    }

    /// Return a session word to the shared pool (called when a peer closes).
    #[cfg(feature = "federation")]
    pub fn release_word(&self, word: &str) {
        self.wordlist.lock().unwrap().release(word);
    }

    /// Whether `pane_id`'s session is proxied from a federated peer rather than
    /// hosted locally. Always `false` without the `federation` feature.
    pub fn is_federated_pane(&self, pane_id: &str) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.is_federated_pane(pane_id)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = pane_id;
            false
        }
    }

    /// Whether the session `word_id` is proxied from a federated peer rather than
    /// hosted locally (issue #146). Always `false` without the `federation`
    /// feature. Session-level analogue of [`ServerApp::is_federated_pane`];
    /// the router asks [`Self::is_forwarded_request`] instead.
    #[cfg(test)]
    pub(crate) fn is_federated_session(&self, word_id: &str) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.is_federated_session(word_id)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = word_id;
            false
        }
    }

    /// Proxy the empty remote session `remote_word` as `local_word` over a
    /// channel pair standing in for a peer link; see
    /// [`PeerManager::install_channel_peer`](crate::federation::PeerManager).
    #[cfg(all(test, feature = "federation"))]
    pub(crate) fn install_channel_peer(
        self: &std::sync::Arc<Self>,
        local_word: &str,
        remote_word: &str,
    ) -> (
        mpsc::UnboundedReceiver<ClientMessage>,
        mpsc::UnboundedSender<ServerMessage>,
    ) {
        self.peer_manager.install_channel_peer(
            self,
            "peer:1",
            local_word,
            remote_word,
            crate::federation::no_reconnect(),
        )
    }

    /// Ensure an upstream connection to `target` exists and surface its sessions
    /// locally, returning the peer's stable [`PeerId`]. Without the feature this
    /// reports a "not supported" error the client already handles.
    pub async fn open_peer(
        self: &std::sync::Arc<Self>,
        target: PeerTarget,
    ) -> Result<PeerId, String> {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.open_peer(self, target).await
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = target;
            Err("federation is not supported by this daemon yet".to_string())
        }
    }

    /// Tear down the upstream connection to `peer` and drop its proxied sessions.
    /// A no-op without the feature (or when the peer is unknown).
    pub fn close_peer(&self, peer: &str) {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.close_peer(self, peer);
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = peer;
        }
    }

    /// Apply connection-pause state (issue #68) to every federated pane `client_id`
    /// views: a paused viewer is skipped in the feed loop's fan-out and resyncs on
    /// resume via re-attach. A no-op without the feature (or for a client viewing no
    /// federated panes). Complements [`ServerApp::set_paused`], which covers
    /// locally-hosted panes.
    pub fn set_federated_paused(&self, client_id: ClientId, paused: bool, auto: bool) {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.set_paused(client_id, paused, auto);
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (client_id, paused, auto);
        }
    }

    /// Exempt (or un-exempt) a single federated pane from `client_id`'s
    /// *auto*-pause (issue #68). Complements
    /// [`ServerApp::set_pane_no_auto_pause`] for locally-hosted panes. A no-op
    /// without the feature or for a pane the client does not view federated.
    pub fn set_federated_pane_no_auto_pause(
        &self,
        client_id: ClientId,
        pane_id: &str,
        exempt: bool,
    ) {
        #[cfg(feature = "federation")]
        {
            self.peer_manager
                .set_pane_no_auto_pause(client_id, pane_id, exempt);
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (client_id, pane_id, exempt);
        }
    }

    /// Tear down every federated peer (abort feed loops, kill SSH tunnels) for
    /// daemon shutdown, so no `ssh -L` child is orphaned when the process exits. A
    /// no-op without the feature (or when no peers are open).
    pub fn close_all_peers(&self) {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.close_all();
        }
    }

    /// The proxied sessions of every open peer, with local IDs and peer-decorated
    /// names, to be merged into [`ServerApp::list_sessions`]. Empty without the
    /// feature.
    pub fn list_federated_sessions(&self) -> Vec<SessionEntry> {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.list_sessions()
        }
        #[cfg(not(feature = "federation"))]
        {
            Vec::new()
        }
    }

    /// Call `publish` with [`Self::list_federated_sessions`] while no peer's
    /// sessions can be added or removed, so what it sends is ordered against
    /// every such change's own event (issue #208).
    pub fn publish_federated_sessions<T>(&self, publish: impl FnOnce(Vec<SessionEntry>) -> T) -> T {
        #[cfg(feature = "federation")]
        let _membership = self.peer_manager.membership();
        publish(self.list_federated_sessions())
    }

    /// The process overview of every open peer (issue #122), with pane ids
    /// translated to local form, to be merged into the hub's
    /// `ProcessOverviewResult`. Empty without the feature.
    pub async fn collect_federated_process_overview(&self) -> Vec<PaneProcesses> {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.collect_process_overview().await
        }
        #[cfg(not(feature = "federation"))]
        {
            Vec::new()
        }
    }

    /// Whether `msg` is a request this hub forwards to a peer (see
    /// [`kmux_protocol::messages::Federation`]): it names a session the hub
    /// proxies, or asks a peer to create one. Without the feature only the
    /// latter, which [`Self::forward_to_peer`] then refuses.
    pub fn is_forwarded_request(&self, msg: &mut ClientMessage) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.is_forwarded(msg)
        }
        #[cfg(not(feature = "federation"))]
        {
            matches!(
                msg.federation(),
                kmux_protocol::messages::Federation::Forward {
                    target: kmux_protocol::messages::Target::Peer(_),
                    ..
                }
            )
        }
    }

    /// Forward the request `msg` from `from` to its peer, whose answer is
    /// routed back to `from` alone (issue #227). Refused at once when it
    /// cannot be forwarded, or without the feature.
    pub fn forward_to_peer(&self, from: Requester, msg: ClientMessage) -> Result<(), Refusal> {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.forward(from, msg)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = from;
            let mut msg = msg;
            let request_id =
                if let kmux_protocol::messages::Federation::Forward { request_id, .. } =
                    msg.federation()
                {
                    request_id.as_deref().copied()
                } else {
                    None
                };
            Err(Refusal {
                request_id,
                code: ErrorCode::InternalError,
                message: "federation is not supported by this daemon yet".to_string(),
            })
        }
    }

    /// Register a viewer of federated `pane_id` and forward an `Attach` upstream.
    /// `data_tx` is the viewer's bounded pane-stream channel; `ctrl_tx` is its
    /// control lane (never dropped), over which a `Lagged` is delivered out-of-band if
    /// the data channel backs up (matching the local relay). Returns `true` when
    /// `pane_id` is federated (and the attach was forwarded), `false` otherwise.
    pub fn federated_attach(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: OutboundTx,
        size: TermSize,
    ) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager
                .attach_viewer(pane_id, client_id, data_tx, ctrl_tx, size)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (pane_id, client_id, data_tx, ctrl_tx, size);
            false
        }
    }

    /// Update `client_id`'s declared size for a federated pane and reconcile the
    /// smallest-wins size upstream. Returns `true` when `pane_id` is federated.
    pub fn federated_resize(&self, pane_id: &str, client_id: ClientId, size: TermSize) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.resize_viewer(pane_id, client_id, size)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (pane_id, client_id, size);
            false
        }
    }

    /// Detach `client_id` from `pane_id`, routing to the peer subsystem when the
    /// pane is federated and to the local relay otherwise.
    pub async fn detach_pane_any(&self, pane_id: &str, client_id: ClientId) {
        #[cfg(feature = "federation")]
        if self.peer_manager.is_federated_pane(pane_id) {
            self.peer_manager.detach_viewer(pane_id, client_id);
            return;
        }
        self.detach_from_pane(pane_id, client_id).await;
    }
}

/// Who sent a message up the link: a client of this hub, or the hub itself
/// (a re-attach after a relink, a refresh of the peer's list, a release of a
/// departed client's input lock). What the peer says to the hub goes nowhere.
#[derive(Clone)]
#[cfg_attr(
    not(feature = "federation"),
    expect(
        dead_code,
        reason = "only the federation subsystem answers a requester"
    )
)]
pub(crate) enum Requester {
    Hub,
    Client { id: ClientId, ctrl: OutboundTx },
}

#[cfg_attr(
    not(feature = "federation"),
    expect(
        dead_code,
        reason = "only the federation subsystem answers a requester"
    )
)]
impl Requester {
    /// The client `id`, answered on its control lane `ctrl`.
    pub(crate) fn client(id: ClientId, ctrl: OutboundTx) -> Self {
        Self::Client { id, ctrl }
    }

    /// Whether `self` and `other` are the same sender: the hub, or one
    /// client over one channel.
    pub(crate) fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Hub, Self::Hub) => true,
            (Self::Client { id: a, ctrl: x }, Self::Client { id: b, ctrl: y }) => {
                a == b && x.same_channel(y)
            }
            _ => false,
        }
    }

    /// Answer the sender with `msg` on its control lane. The hub answers
    /// itself nothing.
    pub(crate) fn answer(&self, msg: ServerMessage) {
        if let Self::Client { ctrl, .. } = self {
            let _ = ctrl.send(msg);
        }
    }
}

/// A forwarded request the hub answers itself, at once, with an `Error`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The request's own id, when it has one.
    pub(crate) request_id: Option<RequestId>,
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl Refusal {
    /// The `Error` a client is sent for it.
    pub(crate) fn into_error(self) -> ServerMessage {
        ServerMessage::Error {
            request_id: self.request_id,
            code: self.code,
            message: self.message,
        }
    }
}
