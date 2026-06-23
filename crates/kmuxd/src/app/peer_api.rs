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
    ClientId, ClientMessage, PaneProcesses, PeerId, PeerTarget, SequenceNo, ServerMessage,
    SessionEntry, TermSize,
};
use tokio::sync::mpsc;

use super::ServerApp;

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

    /// Ensure an upstream connection to `target` exists and surface its sessions
    /// locally, returning the peer's stable [`PeerId`]. Without the feature this
    /// reports a "not supported" error the client already handles.
    pub async fn open_peer(&self, target: PeerTarget) -> Result<PeerId, String> {
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

    /// Create a new session on an already-federated `peer`, returning the
    /// localized [`SessionEntry`] the hub replies to the requesting client.
    /// Without the feature this reports a "not supported" error.
    pub async fn create_remote_session(
        &self,
        peer: &str,
        name: Option<String>,
        cwd: Option<String>,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    ) -> Result<SessionEntry, String> {
        #[cfg(feature = "federation")]
        {
            self.peer_manager
                .create_remote_session(self, peer, name, cwd, program, args, size)
                .await
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (peer, name, cwd, program, args, size);
            Err("federation is not supported by this daemon yet".to_string())
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

    /// If `pane_id` is federated, translate it to its remote pane ID, forward
    /// `build(remote_pane_id)` to the owning peer, and return `true`. Otherwise
    /// return `false` and forward nothing (the caller handles it locally).
    pub fn forward_peer_message(
        &self,
        pane_id: &str,
        build: impl FnOnce(String) -> ClientMessage,
    ) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager.forward_message(pane_id, build)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (pane_id, build);
            false
        }
    }

    /// Register a viewer of federated `pane_id` and forward an `Attach` upstream.
    /// `data_tx` is the viewer's bounded pane-stream channel; `ctrl_tx` is its
    /// unbounded control channel, over which a `Lagged` is delivered out-of-band if
    /// the data channel backs up (matching the local relay). Returns `true` when
    /// `pane_id` is federated (and the attach was forwarded), `false` otherwise.
    pub fn federated_attach(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        last_seqno: Option<SequenceNo>,
        size: TermSize,
    ) -> bool {
        #[cfg(feature = "federation")]
        {
            self.peer_manager
                .attach_viewer(pane_id, client_id, data_tx, ctrl_tx, last_seqno, size)
        }
        #[cfg(not(feature = "federation"))]
        {
            let _ = (pane_id, client_id, data_tx, ctrl_tx, last_seqno, size);
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
