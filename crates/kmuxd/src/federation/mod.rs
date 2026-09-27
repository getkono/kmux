//! Daemon federation (issue #121): the local `kmuxd` opens an upstream
//! connection to a remote `kmuxd` and proxies the remote's sessions to local
//! GUIs, so N windows on a remote host cost **one** network connection instead
//! of N. See `docs/architecture-federation.md`.
//!
//! # Model
//!
//! [`PeerManager`] (held on [`ServerApp`]) owns one
//! [`PeerConnection`] per distinct remote daemon, keyed by [`PeerId`]. Each
//! connection holds the upstream `ClientMessage` sink (`client_tx`), a
//! bidirectional `remote_word ↔ local_word` map, the proxied sessions (with
//! local IDs), and a [`ProxiedPane`] per shared pane (its local viewers, their
//! sizes, and a `CellGrid` mirror). A per-peer **feed loop** drains the upstream
//! `ServerMessage` stream, translates each frame's IDs from remote to local, and
//! fans it out: pane content to that pane's viewers (feeding the mirror), and
//! session-scoped events (titles, layout, lifecycle) to every viewer under the
//! affected word.
//!
//! Federated sessions are kept **entirely separate** from `ServerApp.sessions`
//! (which is strictly PTY-backed): a proxied pane has no local PTY, `term_state`
//! or scrollback, so it must never flow through the PTY relay machinery. The
//! daemon translates IDs at the dispatch boundary and forwards everything else
//! verbatim — the remote daemon needs no awareness of federation and sees the
//! local daemon as one ordinary client.
//!
//! Multiple local GUIs share one proxied pane over a single upstream link, with
//! smallest-wins sizing (the upstream pane size is the `min` over viewers) and
//! zero-round-trip late attach (a second viewer is served a snapshot minted from
//! the mirror). Remaining reconciliation facets — pause-union, capability merge,
//! and input-lock arbitration across local viewers — are tracked in
//! `docs/architecture-federation.md`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{
    ClientId, ClientInfo, ClientMessage, PaneProcesses, PeerId, PeerTarget, RequestId, SequenceNo,
    ServerMessage, SessionEntry, SessionEventMsg, TermSize, epoch_millis,
};
use kmux_protocol::{format_pane_id, parse_pane_id};

mod feed;
pub(crate) mod link;
mod translate;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use translate::localize_entry;

use crate::app::ServerApp;

/// Lock a peer connection, past a poisoned lock: every update to it is a
/// whole-field write, which a panic elsewhere cannot leave half done.
fn lock(conn: &Mutex<PeerConnection>) -> std::sync::MutexGuard<'_, PeerConnection> {
    conn.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How long the hub waits for its peer's `AuthResult`, session list, created
/// session and process overview. The overview's is short: the overview polls
/// ~1 Hz and a slow or dead peer should not stall the whole snapshot — it just
/// contributes nothing this round (issue #122). See `docs/protocol.md`.
use kmux_protocol::timing::{
    AUTH_REPLY_TIMEOUT as AUTH_TIMEOUT, PEER_CREATE_TIMEOUT as CREATE_TIMEOUT,
    PEER_LIST_TIMEOUT as LIST_TIMEOUT, PEER_OVERVIEW_TIMEOUT as OVERVIEW_TIMEOUT,
};

/// Owns every upstream peer connection and routes federated traffic.
#[derive(Default)]
pub struct PeerManager {
    /// Open peers keyed by their stable [`PeerId`].
    peers: Mutex<HashMap<PeerId, Arc<Mutex<PeerConnection>>>>,
    /// `local_word -> PeerId`, so the dispatch layer can resolve a federated
    /// pane to its owning peer with a single lookup.
    word_index: Mutex<HashMap<String, PeerId>>,
    /// Held while a peer's sessions are added or removed (with the broadcast
    /// of that change) and while every client is sent the session list, so
    /// neither overtakes the other: a list taken before a session closed can
    /// never reach clients after its `SessionClosed` and bring it back
    /// (issue #208). Taken before `peers`, a connection or `word_index`.
    membership: Mutex<()>,
}

/// One local GUI viewing a proxied pane: its bounded data channel, its ctrl
/// lane (out-of-band signalling that bypasses data backpressure, e.g.
/// `Lagged`), and its declared terminal size (used for smallest-wins upstream
/// reconciliation).
struct Viewer {
    data_tx: mpsc::Sender<ServerMessage>,
    ctrl_tx: crate::outbound::OutboundTx,
    size: TermSize,
    /// Connection-pause state (issue #68). While paused this viewer receives no
    /// terminal-output frames (it is skipped in `fan_out`, never marked lagged) and
    /// catches up on resume via re-attach; it still counts toward `effective_size`,
    /// exactly as a paused client does in the local PTY relay.
    paused: bool,
    /// When `paused`, whether the pause is the background auto-pause (`true`) vs a
    /// manual pause (`false`); mirrors `ClientSender.pause_auto` (issue #68).
    pause_auto: bool,
    /// When true, this proxied pane is exempt from *auto*-pause for this viewer:
    /// it keeps streaming through a background auto-pause, but a manual pause
    /// still stops it. Mirrors `ClientSender.no_auto_pause` (issue #68).
    no_auto_pause: bool,
}

impl Viewer {
    /// Whether terminal-output frames for this proxied pane should be withheld
    /// from this viewer — see [`crate::app::ClientSender::output_paused`].
    fn output_paused(&self) -> bool {
        self.paused && !(self.pause_auto && self.no_auto_pause)
    }
}

/// A proxied pane: the local viewers sharing it, a [`CellGrid`] mirror fed from
/// the upstream stream (so a late local attacher can be served a snapshot with no
/// upstream round-trip), the upstream seqno the mirror is current to, and the
/// effective size last requested upstream.
struct ProxiedPane {
    viewers: HashMap<ClientId, Viewer>,
    mirror: CellGrid,
    /// Upstream seqno the mirror reflects; stamped onto minted snapshots so a late
    /// attacher's subsequent diffs line up without a spurious gap.
    last_seqno: SequenceNo,
    /// Effective size (smallest-wins over `viewers`) last sent upstream; lets us
    /// suppress redundant upstream `Resize`s.
    upstream_size: TermSize,
}

impl ProxiedPane {
    fn new(size: TermSize) -> Self {
        Self {
            viewers: HashMap::new(),
            mirror: CellGrid::new(size.rows.max(1) as usize, size.cols.max(1) as usize),
            last_seqno: SequenceNo(0),
            upstream_size: size,
        }
    }

    /// Smallest-wins size across all viewers (mirrors `kmuxd`'s `effective_size`).
    /// Zero dims are ignored; pixel dims are not reconciled (the remote sizes by
    /// rows/cols).
    fn effective_size(&self) -> TermSize {
        let rows = self
            .viewers
            .values()
            .map(|v| v.size.rows)
            .filter(|&r| r > 0)
            .min()
            .unwrap_or(0);
        let cols = self
            .viewers
            .values()
            .map(|v| v.size.cols)
            .filter(|&c| c > 0)
            .min()
            .unwrap_or(0);
        TermSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// The viewers' **ctrl** senders — the delivery path for session
    /// events and lifecycle signals, which must not be subject to the per-pane data
    /// backpressure (matching the local daemon, where events reach clients via the
    /// connection's control lane, not the bounded pane stream).
    fn viewer_ctrl_senders(&self) -> Vec<crate::outbound::OutboundTx> {
        self.viewers.values().map(|v| v.ctrl_tx.clone()).collect()
    }

    /// Fan an (already local-addressed) pane frame out to every viewer, applying
    /// the same backpressure policy as the local PTY relay
    /// (`crate::relay::broadcast_to_clients`): a viewer whose **bounded** data
    /// channel is full is sent a [`ServerMessage::Lagged`] over its **ctrl**
    /// lane and dropped — it re-attaches and is served a fresh snapshot
    /// minted off the still-correct mirror, exactly as a lagging local client
    /// recovers. A viewer whose channel has closed is dropped silently. The mirror
    /// is fed by the caller *before* this, so a dropped viewer never desyncs it.
    fn fan_out(&mut self, local_pane_id: &str, msg: &ServerMessage) {
        let mut dead: Vec<ClientId> = Vec::new();
        for (&client_id, viewer) in &self.viewers {
            // Paused viewers (issue #68) receive no terminal output and must never
            // be marked lagged or dropped when their channel fills — they resync on
            // resume via re-attach. Same rule as `relay::broadcast_to_clients`; an
            // auto-pause-exempt pane keeps streaming through a background pause.
            if viewer.output_paused() {
                continue;
            }
            match viewer.data_tx.try_send(msg.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // Out-of-band so it lands even though the data channel is full;
                    // the client re-attaches with its last seqno and resyncs.
                    let _ = viewer.ctrl_tx.send(ServerMessage::Lagged {
                        pane_id: local_pane_id.to_string(),
                        missed_count: 1,
                    });
                    dead.push(client_id);
                    warn!(
                        ?client_id,
                        pane = local_pane_id,
                        "federated viewer lagged; sent Lagged via ctrl and dropped",
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => dead.push(client_id),
            }
        }
        for id in &dead {
            self.viewers.remove(id);
        }
    }

    /// Feed an inbound (already-local-addressed) pane frame into the mirror.
    fn apply_to_mirror(&mut self, msg: &ServerMessage) {
        match msg {
            ServerMessage::TerminalSnapshot {
                snapshot, seqno, ..
            } => {
                self.mirror.apply_snapshot((**snapshot).clone());
                self.last_seqno = *seqno;
            }
            ServerMessage::TerminalUpdate { diff, seqno, .. } => {
                self.mirror.apply_diff((**diff).clone());
                self.last_seqno = *seqno;
            }
            ServerMessage::CursorUpdate {
                cursor,
                modes,
                seqno,
                ..
            } => {
                self.mirror.apply_cursor_update(*cursor, *modes);
                self.last_seqno = *seqno;
            }
            ServerMessage::ScrollbackAppend {
                first_index,
                lines,
                seqno,
                ..
            } => {
                self.mirror
                    .apply_scrollback_append(*first_index, lines.clone());
                self.last_seqno = *seqno;
            }
            _ => {}
        }
    }
}

/// One upstream connection to a remote `kmuxd` and the local state proxying it.
struct PeerConnection {
    /// Upstream sink: send `ClientMessage`s to the remote daemon.
    client_tx: mpsc::UnboundedSender<ClientMessage>,
    /// `remote_word -> local_word` (for translating inbound frames).
    remote_to_local: HashMap<String, String>,
    /// `local_word -> remote_word` (for translating outbound requests).
    local_to_remote: HashMap<String, String>,
    /// Proxied sessions keyed by local word, already localized (local IDs +
    /// peer-decorated name) for [`ServerApp::list_sessions`].
    sessions: HashMap<String, SessionEntry>,
    /// Proxied panes that have at least one local viewer, keyed by local pane ID.
    panes: HashMap<String, ProxiedPane>,
    /// The feed-loop task draining the upstream stream; aborted on close.
    feed_task: Option<JoinHandle<()>>,
    /// Monotonic request-id source for hub-initiated upstream requests (e.g.
    /// create-on-peer). Starts at 2 so it never collides with the `SessionList`
    /// probe (id 1) `open_peer` sends during the handshake.
    next_request_id: RequestId,
    /// In-flight hub-initiated `SessionCreate`s, keyed by upstream request id.
    /// The feed loop completes the oneshot with the remote `SessionEntry` (or an
    /// error string) when the matching `SessionCreated`/`Error` arrives;
    /// `create_remote_session` then draws the local word and registers it.
    pending_creates: HashMap<RequestId, oneshot::Sender<Result<SessionEntry, String>>>,
    /// In-flight hub-initiated `ProcessOverview`s, keyed by upstream request id
    /// (issue #122). The feed loop completes the oneshot with the peer's per-pane
    /// process trees — already translated to local pane ids — when the matching
    /// `ProcessOverviewResult` arrives.
    pending_overviews: HashMap<RequestId, oneshot::Sender<Vec<PaneProcesses>>>,
    /// In-flight hub-initiated `ClientList`s, keyed by upstream request id (issue
    /// #146). The feed loop completes the oneshot with the peer's connections when
    /// the matching `ClientListResult` arrives.
    pending_client_lists: HashMap<RequestId, oneshot::Sender<Vec<ClientInfo>>>,
    /// In-flight hub-initiated requests whose only answer is "done" or an error —
    /// `KickClient` (issue #146), `SessionClose` and `TabClose` — keyed by
    /// upstream request id. The feed loop completes the oneshot when the matching
    /// `ClientKicked`/`SessionClosed`/`TabClosed` (Ok) or `Error` (Err) arrives.
    pending_acks: HashMap<RequestId, oneshot::Sender<Result<(), String>>>,
    /// The background `ssh -L -N` tunnel process for an [`PeerTarget::Ssh`] peer,
    /// kept alive for the life of the connection (the `-L` forward dies with it).
    /// `None` for a [`PeerTarget::Direct`] peer. Killed on close/reap.
    ssh_tunnel: Option<tokio::process::Child>,
    /// The link is down: the peer is unreachable and its link is being
    /// re-opened (issue #208). Its sessions stay listed, flagged
    /// `peer_unreachable`, and requests for them fail at once.
    dead: bool,
    /// How the link is re-opened when it drops. Read afresh on every attempt,
    /// so re-opening an unreachable peer with a new target (a Direct peer's
    /// rotated token) takes effect on the next attempt (issue #208).
    connector: link::Connector,
}

impl PeerConnection {
    fn new(client_tx: mpsc::UnboundedSender<ClientMessage>, connector: link::Connector) -> Self {
        Self {
            connector,
            client_tx,
            remote_to_local: HashMap::new(),
            local_to_remote: HashMap::new(),
            sessions: HashMap::new(),
            panes: HashMap::new(),
            feed_task: None,
            next_request_id: 2,
            pending_creates: HashMap::new(),
            pending_overviews: HashMap::new(),
            pending_client_lists: HashMap::new(),
            pending_acks: HashMap::new(),
            ssh_tunnel: None,
            dead: false,
        }
    }

    /// Re-attach every proxied pane still held (those with a viewer, whose
    /// session the peer still lists) on a re-opened link (issue #208), asking
    /// for a snapshot: the peer may be a new daemon run, whose seqnos start
    /// over. The snapshot re-seeds the mirror and is fanned out like any
    /// frame, so it resyncs every streaming viewer; a paused one catches up
    /// from the mirror when it resumes and re-attaches.
    fn reattach_panes(&self) {
        for (local_pane, pane) in &self.panes {
            let Some((local_word, idx)) = parse_pane_id(local_pane) else {
                continue;
            };
            let Some(remote_word) = self.local_to_remote.get(local_word) else {
                continue;
            };
            let _ = self.client_tx.send(ClientMessage::Attach {
                pane_id: format_pane_id(remote_word, idx),
                last_seqno: None,
                size: pane.upstream_size,
            });
        }
    }

    /// Keep the cached tab `tab_index` of `local_word` in line with a layout
    /// update the peer sent, so the next session list shows it (issue #208).
    fn cache_layout(
        &mut self,
        local_word: &str,
        tab_index: u32,
        layout: &kmux_protocol::messages::LayoutNode,
        focused_pane: u32,
    ) {
        if let Some(tab) = self
            .sessions
            .get_mut(local_word)
            .and_then(|entry| entry.tabs.iter_mut().find(|t| t.tab_index == tab_index))
        {
            tab.layout = layout.clone();
            tab.focused_pane = focused_pane;
        }
    }

    /// Allocate the next upstream request id for a hub-initiated request.
    fn next_rid(&mut self) -> RequestId {
        let rid = self.next_request_id;
        self.next_request_id += 1;
        rid
    }

    /// Record a remote session under a freshly-drawn local word.
    fn register_session(
        &mut self,
        local_word: String,
        remote_word: String,
        remote_entry: SessionEntry,
        peer_id: &str,
    ) {
        let entry = localize_entry(remote_entry, &local_word, peer_id);
        self.remote_to_local
            .insert(remote_word.clone(), local_word.clone());
        self.local_to_remote.insert(local_word.clone(), remote_word);
        self.sessions.insert(local_word, entry);
    }

    /// Translate a remote pane ID (`remote_word/idx`) to its local form.
    fn to_local_pane(&self, remote_pane: &str) -> Option<String> {
        let (remote_word, idx) = parse_pane_id(remote_pane)?;
        let local_word = self.remote_to_local.get(remote_word)?;
        Some(format_pane_id(local_word, idx))
    }

    /// The **ctrl** senders of every viewer of every proxied pane under
    /// `local_word` — the routing target for session-scoped events (titles, layout,
    /// lifecycle), which in local `kmuxd` reach all clients viewing a session, not
    /// just one pane, and are delivered out-of-band (control lane) so backpressure on a
    /// pane's content stream can never drop a title change or a `SessionClosed`.
    fn viewers_under_word(&self, local_word: &str) -> Vec<crate::outbound::OutboundTx> {
        let prefix = format!("{local_word}/");
        self.panes
            .iter()
            .filter(|(pane_id, _)| pane_id.starts_with(&prefix))
            .flat_map(|(_, pane)| pane.viewer_ctrl_senders())
            .collect()
    }

    /// Recompute the smallest-wins size for `local_pane_id` and, if it differs
    /// from what was last sent upstream, forward a single `Resize` to `remote_pane`.
    fn reconcile_size(&mut self, local_pane_id: &str, remote_pane: &str) {
        let new_size = match self.panes.get_mut(local_pane_id) {
            Some(pane) => {
                let eff = pane.effective_size();
                if eff == pane.upstream_size {
                    return;
                }
                pane.upstream_size = eff;
                eff
            }
            None => return,
        };
        let _ = self.client_tx.send(ClientMessage::Resize {
            pane_id: remote_pane.to_string(),
            size: new_size,
        });
    }
}

impl Drop for PeerConnection {
    /// Defence in depth against orphaning an `ssh -L` tunnel. Every explicit
    /// teardown (`close_peer`/`reap_dead_peer`/`PeerManager::close_all`) already
    /// kills the tunnel synchronously, but `tokio::process::Child` is not
    /// kill-on-drop, so if a `PeerConnection` is ever dropped by some other path
    /// its tunnel child would keep running. Killing it here makes "a
    /// `PeerConnection` never leaks its tunnel" a structural invariant. The feed
    /// loop cannot be aborted from here (it holds an `Arc` to this connection, so
    /// this `drop` only runs once it has already ended or been aborted).
    fn drop(&mut self) {
        if let Some(mut child) = self.ssh_tunnel.take() {
            let _ = child.start_kill();
        }
    }
}

/// Kills a parked SSH `-L` tunnel on drop unless [`disarm`](Self::disarm)ed.
/// `tokio::process::Child` is not kill-on-drop, so any error between
/// `ssh::negotiate` and a fully-registered peer would otherwise leak the
/// process. Disarmed once the tunnel is parked on the live [`PeerConnection`].
pub(crate) struct TunnelGuard(pub(crate) Option<tokio::process::Child>);

impl TunnelGuard {
    /// Take the child out, disarming the guard (the caller owns teardown now).
    pub(super) fn disarm(&mut self) -> Option<tokio::process::Child> {
        self.0.take()
    }
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.start_kill();
        }
    }
}

impl PeerManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `peer_id` is open already, so [`Self::open_peer`] reuses it. An
    /// unreachable one is re-opened with `target` from its next attempt on:
    /// the peer may be back under a new token (issue #208).
    fn reuse_peer(&self, peer_id: &str, target: &PeerTarget) -> bool {
        let Some(conn) = self.peers.lock().unwrap().get(peer_id).cloned() else {
            return false;
        };
        let retargeted = {
            let mut guard = lock(&conn);
            if guard.dead {
                guard.connector = link::connector_for(target.clone());
            }
            guard.dead
        };
        debug!(%peer_id, retargeted, "reusing an open peer connection");
        true
    }

    /// Ensure an upstream connection to `target` exists and surface its sessions
    /// locally, returning the peer's [`PeerId`]. Idempotent: an open peer is
    /// reused, reachable or not — an unreachable one's link is being re-opened
    /// already (issue #208), from now on with `target`. Performs the connect +
    /// auth + session-list handshake inline, so the caller awaits a
    /// fully-registered peer before replying `PeerOpened`; from then on the
    /// peer's link supervisor keeps the link up.
    pub async fn open_peer(
        &self,
        app: &Arc<ServerApp>,
        target: PeerTarget,
    ) -> Result<PeerId, String> {
        let peer_id = target.peer_id();
        if self.reuse_peer(&peer_id, &target) {
            return Ok(peer_id);
        }

        let link::Upstream {
            client_tx,
            server_rx,
            sessions: remote_sessions,
            mut tunnel,
        } = link::connect_upstream(target.clone()).await?;

        // Register each remote session under a fresh local word. Park the SSH
        // tunnel (if any) on the connection — disarming the guard — so it lives
        // as long as the link and is killed by `close_peer`.
        let mut conn = PeerConnection::new(client_tx, link::connector_for(target));
        conn.ssh_tunnel = tunnel.disarm();
        let mut assigned_words: Vec<String> = Vec::new();
        for entry in remote_sessions {
            let remote_word = entry.meta.word_id.clone();
            let Some(local_word) = app.draw_word() else {
                for w in &assigned_words {
                    app.release_word(w);
                }
                return Err("local session word pool exhausted".to_string());
            };
            assigned_words.push(local_word.clone());
            conn.register_session(local_word, remote_word, entry, &peer_id);
        }

        if self.publish_peer(app, &peer_id, conn, server_rx, &assigned_words) {
            info!(%peer_id, sessions = assigned_words.len(), "federated peer opened");
        }
        Ok(peer_id)
    }

    /// Publish `conn` as `peer_id`, with the words it drew; whether it won.
    ///
    /// The reuse check at the top of `open_peer` is not atomic with this
    /// insert across the `await`-heavy connect, so two GUIs federating the
    /// *same* target concurrently can both reach here. The winner is decided
    /// under a single `peers` lock — the winner spawns its link and inserts; a
    /// loser tears its duplicate down (closing the redundant upstream link +
    /// SSH tunnel and releasing its drawn words) and reuses the winner — so a
    /// race can never leak a connection or corrupt the word index. The word
    /// index is published only by the winner, while still holding `peers`, so
    /// a lookup that sees a word also finds its connection.
    fn publish_peer(
        &self,
        app: &Arc<ServerApp>,
        peer_id: &PeerId,
        conn: PeerConnection,
        server_rx: mpsc::UnboundedReceiver<ServerMessage>,
        assigned_words: &[String],
    ) -> bool {
        let conn = Arc::new(Mutex::new(conn));
        let won = {
            let mut peers = self.peers.lock().unwrap();
            if peers.contains_key(peer_id) {
                false
            } else {
                let task = link::spawn_link(
                    Arc::downgrade(app),
                    Arc::clone(&conn),
                    peer_id.clone(),
                    server_rx,
                );
                conn.lock().unwrap().feed_task = Some(task);
                peers.insert(peer_id.clone(), Arc::clone(&conn));
                let mut idx = self.word_index.lock().unwrap();
                for w in assigned_words {
                    idx.insert(w.clone(), peer_id.clone());
                }
                true
            }
        };

        if !won {
            // Lost the race: tear down this duplicate (no link was spawned, so
            // dropping `conn` closes its `client_tx` and the upstream link) and
            // return the drawn words to the pool.
            if let Some(mut child) = conn.lock().unwrap().ssh_tunnel.take() {
                let _ = child.start_kill();
            }
            for w in assigned_words {
                app.release_word(w);
            }
            debug!(%peer_id, "lost concurrent open race; discarded duplicate peer link");
        }
        won
    }

    /// Bring the hub's listing of `peer_id`'s sessions in line with the list
    /// the peer just sent (issue #208) — on a re-opened link, or when the
    /// peer's sessions changed. A session the peer still lists keeps its local
    /// word (its entry refreshed); a new one draws a word; one the peer no
    /// longer lists is closed here, for every client. Nothing, once the user
    /// has closed the peer.
    fn reconcile_sessions(
        &self,
        app: &ServerApp,
        conn: &Arc<Mutex<PeerConnection>>,
        peer_id: &str,
        listed: Vec<SessionEntry>,
    ) {
        let _membership = self.membership();
        if self.is_open(peer_id, conn) {
            self.reconcile_sessions_gated(app, conn, peer_id, listed);
        }
    }

    /// [`Self::reconcile_sessions`] for a caller already holding the
    /// membership gate and knowing the peer is open.
    fn reconcile_sessions_gated(
        &self,
        app: &ServerApp,
        conn: &Arc<Mutex<PeerConnection>>,
        peer_id: &str,
        listed: Vec<SessionEntry>,
    ) {
        let gone: Vec<String> = {
            let guard = lock(conn);
            guard
                .local_to_remote
                .iter()
                .filter(|(_, remote)| !listed.iter().any(|e| e.meta.word_id == **remote))
                .map(|(local, _)| local.clone())
                .collect()
        };
        for local_word in gone {
            self.unregister_session(app, &local_word);
            app.broadcast_session_event(SessionEventMsg::SessionClosed {
                word_id: local_word,
            });
        }
        for entry in listed {
            let remote_word = entry.meta.word_id.clone();
            let known = lock(conn).remote_to_local.get(&remote_word).cloned();
            if let Some(local_word) = known {
                let localized = localize_entry(entry, &local_word, peer_id);
                lock(conn).sessions.insert(local_word, localized);
                continue;
            }
            let Some(local_word) = app.draw_word() else {
                warn!(%peer_id, "local session word pool exhausted; a peer session is not listed");
                continue;
            };
            lock(conn).register_session(local_word.clone(), remote_word, entry, peer_id);
            self.word_index
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(local_word, peer_id.to_string());
        }
    }

    /// Create a new session on an already-federated peer: forward a
    /// `SessionCreate` upstream, register the result under a fresh local word,
    /// and return the localized [`SessionEntry`] (the hub then replies
    /// `SessionCreated` to the requesting GUI, exactly as for a local create).
    ///
    /// The feed loop owns the upstream stream once a peer is open, so the
    /// response is routed back through a oneshot the loop completes on seeing the
    /// matching `SessionCreated`/`Error`. Errors if the peer is unknown or dead,
    /// the upstream create fails or times out, or the local word pool is empty.
    // Mirrors `ServerApp::create_session`'s parameter list plus the peer link's
    // `&ServerApp`; a spec struct would add indirection for one internal caller.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_remote_session(
        &self,
        app: &ServerApp,
        peer_id: &str,
        name: Option<String>,
        cwd: Option<String>,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    ) -> Result<SessionEntry, String> {
        // Resolve the live peer, allocate an upstream request id, and register a
        // oneshot the feed loop completes when the response arrives.
        let conn = self
            .peers
            .lock()
            .unwrap()
            .get(peer_id)
            .cloned()
            .ok_or_else(|| format!("peer {peer_id} is not connected"))?;
        let (client_tx, rid, rx) = {
            let mut guard = conn.lock().unwrap();
            if guard.dead {
                return Err(format!("peer {peer_id} connection is closed"));
            }
            let rid = guard.next_rid();
            let (tx, rx) = oneshot::channel();
            guard.pending_creates.insert(rid, tx);
            (guard.client_tx.clone(), rid, rx)
        };

        // Forward the create upstream. `peer: None` — we are the remote daemon's
        // client, so it creates the session locally on that host.
        if client_tx
            .send(ClientMessage::SessionCreate {
                request_id: rid,
                name,
                cwd,
                program,
                args,
                size,
                peer: None,
            })
            .is_err()
        {
            conn.lock().unwrap().pending_creates.remove(&rid);
            return Err(format!("peer {peer_id} connection closed before create"));
        }

        // Await the upstream response (the feed loop completes the oneshot).
        let remote_entry = match tokio::time::timeout(CREATE_TIMEOUT, rx).await {
            Ok(Ok(Ok(entry))) => entry,
            Ok(Ok(Err(reason))) => return Err(format!("peer rejected session create: {reason}")),
            Ok(Err(_)) => return Err("peer connection closed during session create".to_string()),
            Err(_) => {
                conn.lock().unwrap().pending_creates.remove(&rid);
                return Err("peer did not confirm session create in time".to_string());
            }
        };

        // Register under a fresh local word and publish it to the word index, so
        // the new session is addressable and its panes route as federated. The
        // peer's `SessionCreated` event may have made the hub re-list the
        // peer's sessions first; then the session is already registered, and
        // keeps the word it got (issue #208).
        let remote_word = remote_entry.meta.word_id.clone();
        // Checked and registered under the gate, so a concurrent re-list
        // cannot register the same session under a second word.
        let _membership = self.membership();
        let registered = {
            let guard = conn.lock().unwrap();
            guard
                .remote_to_local
                .get(&remote_word)
                .and_then(|local| guard.sessions.get(local))
                .cloned()
        };
        if let Some(entry) = registered {
            return Ok(entry);
        }
        let local_word = app
            .draw_word()
            .ok_or_else(|| "local session word pool exhausted".to_string())?;
        let entry = {
            let mut guard = conn.lock().unwrap();
            guard.register_session(local_word.clone(), remote_word, remote_entry, peer_id);
            guard.sessions.get(&local_word).cloned()
        };
        self.word_index
            .lock()
            .unwrap()
            .insert(local_word.clone(), peer_id.to_string());
        info!(%peer_id, local_word, "created session on federated peer");
        entry.ok_or_else(|| "internal: federated session vanished after register".to_string())
    }

    /// Tear down the upstream connection to `peer_id`, release its local words,
    /// and abort its feed loop. No-op when the peer is unknown.
    ///
    /// The user closing the peer is one of the two things that close its
    /// sessions (the other is the peer closing them): every client is sent
    /// `SessionClosed` for each (issue #208).
    pub fn close_peer(&self, app: &ServerApp, peer_id: &str) {
        let _membership = self.membership();
        let conn = self.peers.lock().unwrap().remove(peer_id);
        let Some(conn) = conn else { return };
        let mut guard = conn.lock().unwrap();
        if let Some(task) = &guard.feed_task {
            task.abort();
        }
        if let Some(mut child) = guard.ssh_tunnel.take() {
            let _ = child.start_kill();
        }
        let mut idx = self.word_index.lock().unwrap();
        for local_word in guard.local_to_remote.keys() {
            idx.remove(local_word);
            app.release_word(local_word);
            app.broadcast_session_event(SessionEventMsg::SessionClosed {
                word_id: local_word.clone(),
            });
        }
        info!(%peer_id, "federated peer closed");
    }

    /// Tear every peer down for daemon shutdown: abort each feed loop and kill each
    /// SSH `-L` tunnel **synchronously**, so no tunnel child is orphaned when the
    /// process exits. This matters because `tokio::process::Child` is not
    /// kill-on-drop and the runtime is torn down in the background
    /// (`Runtime::shutdown_background`), which races process exit — so relying on
    /// drop order is not enough. Words are not returned to the pool (the daemon is
    /// going away). Idempotent and a no-op when no peers are open.
    pub fn close_all(&self) {
        let conns: Vec<_> = self
            .peers
            .lock()
            .unwrap()
            .drain()
            .map(|(_, conn)| conn)
            .collect();
        self.word_index.lock().unwrap().clear();
        let count = conns.len();
        for conn in &conns {
            let mut guard = conn.lock().unwrap();
            if let Some(task) = &guard.feed_task {
                task.abort();
            }
            if let Some(mut child) = guard.ssh_tunnel.take() {
                let _ = child.start_kill();
            }
        }
        if count > 0 {
            info!(peers = count, "closed all federated peers on shutdown");
        }
    }

    /// Whether `pane_id`'s session is proxied from a peer.
    pub fn is_federated_pane(&self, pane_id: &str) -> bool {
        match parse_pane_id(pane_id) {
            Some((word, _)) => self.word_index.lock().unwrap().contains_key(word),
            None => false,
        }
    }

    /// Hold the membership gate: no peer's sessions are added or removed
    /// until the guard drops.
    pub(crate) fn membership(&self) -> std::sync::MutexGuard<'_, ()> {
        self.membership
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `conn` is still `peer_id`'s open connection: `close_peer` has
    /// not removed it. Checked under the membership gate, which `close_peer`
    /// holds, by anything that would otherwise register words or park a
    /// tunnel on a closed peer.
    fn is_open(&self, peer_id: &str, conn: &Arc<Mutex<PeerConnection>>) -> bool {
        self.peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(peer_id)
            .is_some_and(|open| Arc::ptr_eq(open, conn))
    }

    /// Proxied sessions across all peers (local IDs, peer-decorated names).
    ///
    /// The sessions of a peer whose link is down are listed too, flagged
    /// `peer_unreachable` (issue #208).
    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        let peers = self.peers.lock().unwrap();
        let mut out = Vec::new();
        for conn in peers.values() {
            let guard = conn.lock().unwrap();
            out.extend(guard.sessions.values().cloned().map(|entry| SessionEntry {
                peer_unreachable: guard.dead,
                ..entry
            }));
        }
        out
    }

    /// Collect the process overview from every connected peer (issue #122),
    /// fanning out one `ProcessOverview` request per peer and awaiting the
    /// replies concurrently. Returned pane ids are already local (the feed loop
    /// translates them). A dead or slow peer contributes nothing this round
    /// rather than stalling the snapshot.
    pub async fn collect_process_overview(&self) -> Vec<PaneProcesses> {
        let conns: Vec<Arc<Mutex<PeerConnection>>> =
            self.peers.lock().unwrap().values().cloned().collect();

        let mut set = tokio::task::JoinSet::new();
        for conn in conns {
            // Allocate a request id, register the oneshot, and dispatch upstream.
            let (rid, rx) = {
                let mut guard = conn.lock().unwrap();
                if guard.dead {
                    continue;
                }
                let rid = guard.next_rid();
                let (tx, rx) = oneshot::channel();
                guard.pending_overviews.insert(rid, tx);
                if guard
                    .client_tx
                    .send(ClientMessage::ProcessOverview { request_id: rid })
                    .is_err()
                {
                    guard.pending_overviews.remove(&rid);
                    continue;
                }
                (rid, rx)
            };
            set.spawn(async move {
                match tokio::time::timeout(OVERVIEW_TIMEOUT, rx).await {
                    Ok(Ok(panes)) => panes,
                    // Timed out or the link closed: drop the registration and
                    // move on, so this peer simply contributes nothing.
                    _ => {
                        conn.lock().unwrap().pending_overviews.remove(&rid);
                        Vec::new()
                    }
                }
            });
        }

        let mut out = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(panes) = res {
                out.extend(panes);
            }
        }
        out
    }

    /// Whether the local session `word_id` is proxied from a federated peer
    /// (issue #146).
    pub fn is_federated_session(&self, word_id: &str) -> bool {
        self.word_index.lock().unwrap().contains_key(word_id)
    }

    /// Forward a `ClientList` for the federated session `local_word` to its owning
    /// peer and return the connections the peer reports (issue #146). The peer's
    /// labels/ids/machine ids are relayed verbatim — they are meaningful on that
    /// host (and `machine_id` is globally unique).
    pub async fn list_session_clients(&self, local_word: &str) -> Result<Vec<ClientInfo>, String> {
        let conn = self
            .conn_for_word(local_word)
            .ok_or_else(|| format!("session {local_word} is not federated"))?;
        let (rid, rx) = {
            let mut guard = conn.lock().unwrap();
            if guard.dead {
                return Err("peer connection is closed".to_string());
            }
            let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
                return Err(format!("session {local_word} is not federated"));
            };
            let rid = guard.next_rid();
            let (tx, rx) = oneshot::channel();
            guard.pending_client_lists.insert(rid, tx);
            if guard
                .client_tx
                .send(ClientMessage::ClientList {
                    request_id: rid,
                    word_id: remote_word,
                })
                .is_err()
            {
                guard.pending_client_lists.remove(&rid);
                return Err("peer connection closed before client list".to_string());
            }
            (rid, rx)
        };
        match tokio::time::timeout(LIST_TIMEOUT, rx).await {
            Ok(Ok(clients)) => Ok(clients),
            _ => {
                conn.lock().unwrap().pending_client_lists.remove(&rid);
                Err("peer did not return a client list in time".to_string())
            }
        }
    }

    /// Forward a `KickClient` for the federated session `local_word` to its owning
    /// peer, translating the local word to the remote one (issue #146).
    pub async fn kick_session_client(
        &self,
        local_word: &str,
        client_id: ClientId,
    ) -> Result<(), String> {
        self.request_ack(local_word, "kick", |request_id, word_id| {
            ClientMessage::KickClient {
                request_id,
                word_id,
                client_id,
            }
        })
        .await
    }

    /// Close the federated session `local_word` on its owning peer, then drop it
    /// from this hub — its word leaves the index and returns to the pool, its
    /// proxied panes and listing go away — and tell every client it closed.
    ///
    /// The drop and the broadcast happen together under the membership gate,
    /// so no session list can carry the session past its `SessionClosed`
    /// (issue #208). They happen only while the word still names the session
    /// that was closed: the peer's own `SessionClosed` event may have got there
    /// first, and the word may since have been drawn again.
    pub async fn close_remote_session(
        &self,
        app: &ServerApp,
        local_word: &str,
    ) -> Result<(), String> {
        let conn = self
            .conn_for_word(local_word)
            .ok_or_else(|| format!("session {local_word} is not federated"))?;
        let remote_word = lock(&conn).local_to_remote.get(local_word).cloned();
        self.request_ack(local_word, "session close", |request_id, word_id| {
            ClientMessage::SessionClose {
                request_id,
                word_id,
            }
        })
        .await?;
        let _membership = self.membership();
        if remote_word.is_some_and(|remote| self.still_names(local_word, &conn, &remote)) {
            self.unregister_session(app, local_word);
            app.broadcast_session_event(SessionEventMsg::SessionClosed {
                word_id: local_word.to_string(),
            });
        }
        Ok(())
    }

    /// Whether `local_word` still names `conn`'s session `remote_word`: not
    /// closed by the peer's own event, nor released by `close_peer` (which
    /// leaves the connection's maps as they were) and drawn again since.
    /// Asked under the membership gate.
    fn still_names(
        &self,
        local_word: &str,
        conn: &Arc<Mutex<PeerConnection>>,
        remote_word: &str,
    ) -> bool {
        self.conn_for_word(local_word)
            .is_some_and(|open| Arc::ptr_eq(&open, conn))
            && lock(conn)
                .local_to_remote
                .get(local_word)
                .is_some_and(|mapped| mapped == remote_word)
    }

    /// Close tab `tab_index` of the federated session `local_word` on its owning
    /// peer. The peer broadcasts the resulting `TabClosed` (or `SessionClosed`,
    /// for its last tab) and the feed loop relays it to this session's viewers.
    pub async fn close_remote_tab(&self, local_word: &str, tab_index: u32) -> Result<(), String> {
        self.request_ack(local_word, "tab close", |request_id, word_id| {
            ClientMessage::TabClose {
                request_id,
                word_id,
                tab_index,
            }
        })
        .await
    }

    /// Translate `local_word` to its remote word and forward `build(remote_word)`
    /// to the owning peer, for a session-scoped message that carries no request
    /// id (the layout nudges). The peer answers with a `LayoutUpdate`, which the
    /// feed loop translates back and relays to this session's viewers.
    pub fn forward_session_message(
        &self,
        local_word: &str,
        build: impl FnOnce(String) -> ClientMessage,
    ) -> Result<(), String> {
        let conn = self
            .conn_for_word(local_word)
            .ok_or_else(|| format!("session {local_word} is not federated"))?;
        // Past a poisoned lock rather than through it: this reads two fields
        // and sends one message, which a panic elsewhere cannot have left
        // half-done, and a layout nudge is no reason to take the daemon down.
        let guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.dead {
            return Err("peer connection is closed".to_string());
        }
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return Err(format!("session {local_word} is not federated"));
        };
        guard
            .client_tx
            .send(build(remote_word))
            .map_err(|_| "peer connection is closed".to_string())
    }

    /// Send `build(request_id, remote_word)` for the federated session
    /// `local_word` upstream and wait for the peer to confirm it or refuse it.
    /// `what` names the request in the errors a user sees.
    async fn request_ack(
        &self,
        local_word: &str,
        what: &str,
        build: impl FnOnce(RequestId, String) -> ClientMessage,
    ) -> Result<(), String> {
        let conn = self
            .conn_for_word(local_word)
            .ok_or_else(|| format!("session {local_word} is not federated"))?;
        let (rid, rx) = {
            let mut guard = conn.lock().unwrap();
            if guard.dead {
                return Err("peer connection is closed".to_string());
            }
            let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
                return Err(format!("session {local_word} is not federated"));
            };
            let rid = guard.next_rid();
            let (tx, rx) = oneshot::channel();
            guard.pending_acks.insert(rid, tx);
            if guard.client_tx.send(build(rid, remote_word)).is_err() {
                guard.pending_acks.remove(&rid);
                return Err(format!("peer connection closed before {what}"));
            }
            (rid, rx)
        };
        match tokio::time::timeout(CREATE_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            _ => {
                conn.lock().unwrap().pending_acks.remove(&rid);
                Err(format!("peer did not confirm {what} in time"))
            }
        }
    }

    /// Forget the federated session `local_word`: its mappings, listing and
    /// proxied panes, and its entry in the word index, returning the word to the
    /// pool. A no-op for a word that is not federated.
    ///
    /// Proceeds past a poisoned lock: it only removes entries, which leaves the
    /// maps consistent whatever state a panic elsewhere left them in.
    fn unregister_session(&self, app: &ServerApp, local_word: &str) {
        let Some(conn) = self.conn_for_word(local_word) else {
            return;
        };
        {
            let mut guard = conn.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(remote_word) = guard.local_to_remote.remove(local_word) {
                guard.remote_to_local.remove(&remote_word);
            }
            guard.sessions.remove(local_word);
            let prefix = format!("{local_word}/");
            guard
                .panes
                .retain(|pane_id, _| !pane_id.starts_with(&prefix));
        }
        self.word_index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(local_word);
        app.release_word(local_word);
    }

    /// Translate `local_pane_id` to its remote form and forward
    /// `build(remote_pane_id)` upstream. Returns `false` (forwarding nothing) when
    /// the pane is not federated, so the caller can fall back to local handling.
    pub fn forward_message(
        &self,
        local_pane_id: &str,
        build: impl FnOnce(String) -> ClientMessage,
    ) -> bool {
        let Some((local_word, idx)) = parse_pane_id(local_pane_id) else {
            return false;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return false;
        };
        let guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word) else {
            return false;
        };
        let remote_pane = format_pane_id(remote_word, idx);
        guard.client_tx.send(build(remote_pane)).is_ok()
    }

    /// Register `data_tx` as a viewer of federated `local_pane_id`. The **first**
    /// viewer of a pane forwards an `Attach` upstream (the remote streams a
    /// snapshot back); a **later** viewer is served a snapshot minted from the live
    /// mirror with **no upstream round-trip**, then the upstream size is reconciled
    /// (smallest-wins). Returns `false` when the pane is not federated.
    ///
    /// The first viewer always asks the remote for a snapshot, whatever seqno
    /// the viewer resumes from (issue #208): the hub's mirror is new and blank,
    /// and a delta on top of it would leave every later viewer a wrong grid.
    /// The viewer is served that snapshot too, which resyncs it.
    pub fn attach_viewer(
        &self,
        local_pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: crate::outbound::OutboundTx,
        size: TermSize,
    ) -> bool {
        let Some((local_word, idx)) = parse_pane_id(local_pane_id) else {
            return false;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return false;
        };
        let mut guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return false;
        };
        let remote_pane = format_pane_id(&remote_word, idx);

        if guard.panes.contains_key(local_pane_id) {
            // Late viewer: mint from the mirror, register, then reconcile size.
            let pane = guard.panes.get_mut(local_pane_id).unwrap();
            let minted = ServerMessage::TerminalSnapshot {
                pane_id: local_pane_id.to_string(),
                snapshot: Arc::new(pane.mirror.to_snapshot()),
                seqno: pane.last_seqno,
                sent_at_ms: epoch_millis(),
            };
            let _ = data_tx.try_send(minted);
            pane.viewers.insert(
                client_id,
                Viewer {
                    data_tx,
                    ctrl_tx,
                    size,
                    paused: false,
                    pause_auto: false,
                    no_auto_pause: false,
                },
            );
            guard.reconcile_size(local_pane_id, &remote_pane);
        } else {
            // First viewer: create the pane (mirror sized to the viewer) and
            // forward Attach upstream; the remote's snapshot arrives via the feed
            // loop and seeds the mirror.
            let mut pane = ProxiedPane::new(size);
            pane.viewers.insert(
                client_id,
                Viewer {
                    data_tx,
                    ctrl_tx,
                    size,
                    paused: false,
                    pause_auto: false,
                    no_auto_pause: false,
                },
            );
            guard.panes.insert(local_pane_id.to_string(), pane);
            let _ = guard.client_tx.send(ClientMessage::Attach {
                pane_id: remote_pane,
                last_seqno: None,
                size,
            });
        }
        true
    }

    /// Update `client_id`'s declared size for federated `local_pane_id` and
    /// reconcile the smallest-wins size upstream. Returns `false` when the pane is
    /// not federated.
    pub fn resize_viewer(&self, local_pane_id: &str, client_id: ClientId, size: TermSize) -> bool {
        let Some((local_word, idx)) = parse_pane_id(local_pane_id) else {
            return false;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return false;
        };
        let mut guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return false;
        };
        if let Some(pane) = guard.panes.get_mut(local_pane_id)
            && let Some(viewer) = pane.viewers.get_mut(&client_id)
        {
            viewer.size = size;
        }
        let remote_pane = format_pane_id(&remote_word, idx);
        guard.reconcile_size(local_pane_id, &remote_pane);
        true
    }

    /// Remove `client_id` as a viewer of federated `local_pane_id`. When it was the
    /// **last** viewer, forward a `Detach` upstream and drop the mirror; otherwise
    /// reconcile the upstream size (a departing viewer may have been the smallest).
    pub fn detach_viewer(&self, local_pane_id: &str, client_id: ClientId) {
        let Some((local_word, idx)) = parse_pane_id(local_pane_id) else {
            return;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return;
        };
        let mut guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return;
        };
        let remote_pane = format_pane_id(&remote_word, idx);
        let became_empty = match guard.panes.get_mut(local_pane_id) {
            Some(pane) => {
                pane.viewers.remove(&client_id);
                pane.viewers.is_empty()
            }
            None => return,
        };
        if became_empty {
            guard.panes.remove(local_pane_id);
            let _ = guard.client_tx.send(ClientMessage::Detach {
                pane_id: remote_pane,
            });
        } else {
            guard.reconcile_size(local_pane_id, &remote_pane);
        }
    }

    /// Apply connection-pause state (issue #68) to every federated pane `client_id`
    /// views, across all peers. A paused viewer is skipped in [`ProxiedPane::fan_out`]
    /// (it stops receiving terminal output and resyncs on resume via re-attach, which
    /// mints from the still-current mirror) but still counts toward smallest-wins
    /// sizing — the same semantics as the local relay's `set_paused`. No-op for a
    /// client that views no federated panes.
    pub fn set_paused(&self, client_id: ClientId, paused: bool, auto: bool) {
        let conns: Vec<_> = self.peers.lock().unwrap().values().cloned().collect();
        for conn in conns {
            let mut guard = conn.lock().unwrap();
            for pane in guard.panes.values_mut() {
                if let Some(viewer) = pane.viewers.get_mut(&client_id) {
                    viewer.paused = paused;
                    viewer.pause_auto = auto;
                }
            }
        }
    }

    /// Exempt (or un-exempt) one proxied pane from this viewer's *auto*-pause
    /// (issue #68); see [`crate::app::ServerApp::set_pane_no_auto_pause`]. The
    /// `local_pane_id` is the client-facing id, which is what `panes` is keyed by.
    pub fn set_pane_no_auto_pause(&self, client_id: ClientId, local_pane_id: &str, exempt: bool) {
        let conns: Vec<_> = self.peers.lock().unwrap().values().cloned().collect();
        for conn in conns {
            let mut guard = conn.lock().unwrap();
            if let Some(pane) = guard.panes.get_mut(local_pane_id)
                && let Some(viewer) = pane.viewers.get_mut(&client_id)
            {
                viewer.no_auto_pause = exempt;
            }
        }
    }

    fn conn_for_word(&self, local_word: &str) -> Option<Arc<Mutex<PeerConnection>>> {
        let peer_id = self.word_index.lock().unwrap().get(local_word).cloned()?;
        self.peers.lock().unwrap().get(&peer_id).cloned()
    }

    /// Publish a peer whose upstream link is a pair of channels instead of a
    /// socket, proxying one empty session `remote_word` as `local_word`, and
    /// re-opened (when it drops) by `connector`. Returns what the hub sends
    /// upstream and the sender that plays the peer's replies into the real
    /// link supervisor.
    #[cfg(test)]
    pub(crate) fn install_channel_peer(
        &self,
        app: &Arc<ServerApp>,
        peer_id: &str,
        local_word: &str,
        remote_word: &str,
        connector: link::Connector,
    ) -> (
        mpsc::UnboundedReceiver<ClientMessage>,
        mpsc::UnboundedSender<ServerMessage>,
    ) {
        let (client_tx, client_rx) = mpsc::unbounded_channel();
        let (server_tx, server_rx) = mpsc::unbounded_channel();
        let mut conn = PeerConnection::new(client_tx, connector);
        conn.register_session(
            local_word.to_string(),
            remote_word.to_string(),
            sample_remote_entry(remote_word),
            peer_id,
        );
        let conn = Arc::new(Mutex::new(conn));
        let task = link::spawn_link(
            Arc::downgrade(app),
            Arc::clone(&conn),
            peer_id.to_string(),
            server_rx,
        );
        conn.lock().unwrap().feed_task = Some(task);
        self.peers.lock().unwrap().insert(peer_id.to_string(), conn);
        self.word_index
            .lock()
            .unwrap()
            .insert(local_word.to_string(), peer_id.to_string());
        (client_rx, server_tx)
    }
}

/// An empty remote session `remote_word`, as a peer lists it.
#[cfg(test)]
pub(crate) fn sample_remote_entry(remote_word: &str) -> SessionEntry {
    use kmux_protocol::messages::{LayoutNode, SessionMeta, TabInfo};
    SessionEntry {
        meta: SessionMeta {
            index: 0,
            word_id: remote_word.to_string(),
            name: remote_word.to_string(),
            cwd: "/".to_string(),
        },
        panes: vec![],
        tabs: vec![TabInfo {
            tab_index: 0,
            name: "1".to_string(),
            layout: LayoutNode::single(0),
            focused_pane: 0,
        }],
        active_tab: 0,
        peer: None,
        peer_unreachable: false,
    }
}

/// A connector that never re-opens a link: for a channel peer whose test does
/// not re-open it.
#[cfg(test)]
pub(crate) fn no_reconnect() -> link::Connector {
    Arc::new(|| Box::pin(async { Err("a channel peer is not re-opened".to_string()) }))
}

/// Receive from `rx` until a message satisfies `pred` or `timeout` elapses,
/// skipping (and dropping) non-matching messages. Used only for the pre-stream
/// handshake; once streaming starts the feed loop owns `rx`.
async fn recv_until(
    rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    timeout: Duration,
    pred: impl Fn(&ServerMessage) -> bool,
) -> Option<ServerMessage> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(m)) if pred(&m) => return Some(m),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::translate::{
        localize_entry, msg_pane_id, rewrite_event_to_local, sendable_clients, set_msg_pane_id,
    };
    use super::*;
    use kmux_protocol::messages::{
        ClientInfo, ConnectionId, FrontendKind, GridSnapshot, PaneInfo, SequenceNo, SessionMeta,
        SessionStatus, TabInfo,
    };

    fn sample_entry(word: &str, name: &str) -> SessionEntry {
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word.to_string(),
                name: name.to_string(),
                cwd: "/tmp".to_string(),
            },
            panes: vec![PaneInfo {
                pane_id: format_pane_id(word, 0),
                pane_index: 0,
                program: "sh".to_string(),
                size: TermSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                attached_clients: vec![ClientId(7)],
                status: SessionStatus::Running,
                title: String::new(),
                progress_state: Default::default(),
                progress: None,
            }],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".to_string(),
                layout: kmux_protocol::messages::LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
            peer_unreachable: false,
        }
    }

    fn snapshot_msg(pane_id: &str) -> ServerMessage {
        ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot: Arc::new(GridSnapshot {
                rows: 1,
                cols: 1,
                cells: vec![],
                cursor: Default::default(),
                modes: kmux_protocol::messages::TermModes::EMPTY,
                history_total: 0,
                scrollback_base: 0,
                scrollback_tail: vec![],
            }),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        }
    }

    #[test]
    fn localize_entry_rewrites_ids_and_decorates_name() {
        let local = localize_entry(sample_entry("eagle", "work"), "hawk", "box:9000");
        assert_eq!(local.meta.word_id, "hawk");
        assert_eq!(local.meta.name, "work @ box:9000");
        assert_eq!(local.peer.as_deref(), Some("box:9000"));
        assert_eq!(local.panes[0].pane_id, "hawk/0");
        assert_eq!(local.panes[0].pane_index, 0);
        // Remote client IDs are meaningless locally and must be cleared.
        assert!(local.panes[0].attached_clients.is_empty());
        // Tabs reference pane_index, not the word, so they survive unchanged.
        assert_eq!(local.tabs[0].tab_index, 0);
    }

    /// A value a newer peer sent that this hub does not know is passed on as
    /// the known value it is shown as, never as `Unknown` (protocol 1.1).
    #[test]
    fn a_hub_passes_unknown_values_on_as_known_ones() {
        use kmux_protocol::messages::{AttentionKind, PaneProgressState};

        let mut remote = sample_entry("eagle", "work");
        remote.panes[0].progress_state = PaneProgressState::Unknown;
        let local = localize_entry(remote, "hawk", "box:9000");
        assert_eq!(local.panes[0].progress_state, PaneProgressState::Remove);

        let map = HashMap::from([("eagle".to_string(), "hawk".to_string())]);
        let mut progress = SessionEventMsg::PaneProgressChanged {
            pane_id: "eagle/0".into(),
            state: PaneProgressState::Unknown,
            progress: Some(3),
        };
        assert_eq!(
            rewrite_event_to_local(&mut progress, &map).as_deref(),
            Some("hawk")
        );
        assert!(matches!(
            progress,
            SessionEventMsg::PaneProgressChanged {
                state: PaneProgressState::Remove,
                ..
            }
        ));
        let mut attention = SessionEventMsg::PaneAttention {
            pane_id: "eagle/0".into(),
            kind: AttentionKind::Unknown,
            title: "t".into(),
            body: "b".into(),
            attention_id: 1,
        };
        assert_eq!(
            rewrite_event_to_local(&mut attention, &map).as_deref(),
            Some("hawk")
        );
        assert!(matches!(
            attention,
            SessionEventMsg::PaneAttention {
                kind: AttentionKind::TurnDone,
                ..
            }
        ));
        let mut unknown = SessionEventMsg::Unknown;
        assert_eq!(rewrite_event_to_local(&mut unknown, &map), None);

        let clients = sendable_clients(vec![
            sample_client_info(FrontendKind::Unknown),
            sample_client_info(FrontendKind::Swift),
        ]);
        let frontends: Vec<_> = clients.iter().map(|c| c.frontend).collect();
        assert_eq!(frontends, vec![FrontendKind::Cli, FrontendKind::Swift]);
    }

    fn sample_client_info(frontend: FrontendKind) -> ClientInfo {
        ClientInfo {
            client_id: ClientId(1),
            connection_id: ConnectionId(1),
            label: "u@h".into(),
            machine_id: "m".into(),
            hostname: "h".into(),
            username: "u".into(),
            transport: "uds".into(),
            attached_panes: vec![],
            uptime_secs: 0,
            is_self: false,
            frontend,
            build: String::new(),
            build_profile: String::new(),
        }
    }

    #[test]
    fn msg_pane_id_round_trips() {
        let mut msg = snapshot_msg("eagle/0");
        assert_eq!(msg_pane_id(&msg), Some("eagle/0"));
        set_msg_pane_id(&mut msg, "hawk/0".to_string());
        assert_eq!(msg_pane_id(&msg), Some("hawk/0"));
        // A message with no pane_id is left alone.
        let ping = ServerMessage::Ping { seq: 1 };
        assert_eq!(msg_pane_id(&ping), None);
    }

    #[test]
    fn rewrite_event_to_local_translates_pane_and_word_events() {
        let mut map = HashMap::new();
        map.insert("eagle".to_string(), "hawk".to_string());

        // A pane-scoped event rewrites the word portion of its pane ID.
        let mut title = SessionEventMsg::PaneTitleChanged {
            pane_id: "eagle/2".into(),
            title: "t".into(),
        };
        assert_eq!(
            rewrite_event_to_local(&mut title, &map).as_deref(),
            Some("hawk")
        );
        match title {
            SessionEventMsg::PaneTitleChanged { pane_id, .. } => assert_eq!(pane_id, "hawk/2"),
            _ => unreachable!(),
        }

        // A word-scoped event rewrites its word ID.
        let mut tab = SessionEventMsg::TabCreated {
            word_id: "eagle".into(),
            tab_index: 1,
        };
        assert_eq!(
            rewrite_event_to_local(&mut tab, &map).as_deref(),
            Some("hawk")
        );
        match tab {
            SessionEventMsg::TabCreated { word_id, .. } => assert_eq!(word_id, "hawk"),
            _ => unreachable!(),
        }

        // An event for an unfederated word is dropped (returns None).
        let mut other = SessionEventMsg::PaneClosed {
            pane_id: "unknown/0".into(),
        };
        assert_eq!(rewrite_event_to_local(&mut other, &map), None);
    }

    #[test]
    fn peer_connection_translates_panes_both_ways() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut conn = PeerConnection::new(tx, no_reconnect());
        conn.register_session(
            "hawk".to_string(),
            "eagle".to_string(),
            sample_entry("eagle", "work"),
            "box:9000",
        );
        // Inbound: remote -> local.
        assert_eq!(conn.to_local_pane("eagle/0").as_deref(), Some("hawk/0"));
        assert_eq!(conn.to_local_pane("eagle/2").as_deref(), Some("hawk/2"));
        assert_eq!(conn.to_local_pane("unknown/0"), None);
        // Outbound mapping is the inverse.
        assert_eq!(
            conn.local_to_remote.get("hawk").map(String::as_str),
            Some("eagle")
        );
    }

    fn sz(rows: u16, cols: u16) -> TermSize {
        TermSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// A test viewer with a bounded data channel and a throwaway ctrl channel.
    fn test_viewer(cap: usize, size: TermSize) -> (Viewer, mpsc::Receiver<ServerMessage>) {
        let (data_tx, data_rx) = mpsc::channel(cap);
        let (ctrl_tx, _ctrl_rx) = crate::fixtures::make_outbound();
        (
            Viewer {
                data_tx,
                ctrl_tx,
                size,
                paused: false,
                pause_auto: false,
                no_auto_pause: false,
            },
            data_rx,
        )
    }

    #[test]
    fn proxied_pane_effective_size_is_smallest_wins() {
        let mut pane = ProxiedPane::new(sz(24, 80));
        let (v1, _r1) = test_viewer(8, sz(24, 80));
        let (v2, _r2) = test_viewer(8, sz(10, 40));
        pane.viewers.insert(ClientId(1), v1);
        pane.viewers.insert(ClientId(2), v2);
        // Smallest-wins across viewers (the size forwarded upstream).
        let eff = pane.effective_size();
        assert_eq!((eff.rows, eff.cols), (10, 40));
        assert_eq!(pane.viewer_ctrl_senders().len(), 2);
    }

    #[test]
    fn proxied_pane_mirror_round_trips_for_late_attach() {
        let mut pane = ProxiedPane::new(sz(1, 1));
        let mut cells = vec![kmux_protocol::messages::CellState::default(); 3];
        cells[0].c = 'X';
        cells[1].c = 'Y';
        cells[2].c = 'Z';
        let msg = ServerMessage::TerminalSnapshot {
            pane_id: "hawk/0".to_string(),
            snapshot: Arc::new(GridSnapshot {
                rows: 1,
                cols: 3,
                cells,
                cursor: Default::default(),
                modes: kmux_protocol::messages::TermModes::EMPTY,
                history_total: 0,
                scrollback_base: 0,
                scrollback_tail: vec![],
            }),
            seqno: SequenceNo(7),
            sent_at_ms: 0,
        };
        pane.apply_to_mirror(&msg);
        // The mirror tracks the upstream seqno (stamped onto a minted snapshot so a
        // late attacher's later diffs line up)…
        assert_eq!(pane.last_seqno, SequenceNo(7));
        // …and a snapshot minted from the mirror carries the applied content.
        let minted = pane.mirror.to_snapshot();
        let text: String = minted.cells.iter().map(|c| c.c).collect();
        assert!(
            text.contains("XYZ"),
            "minted snapshot must carry mirror content: {text:?}"
        );
    }

    #[test]
    fn fan_out_delivers_to_healthy_viewer() {
        let mut pane = ProxiedPane::new(sz(24, 80));
        let (v, mut rx) = test_viewer(8, sz(24, 80));
        pane.viewers.insert(ClientId(1), v);

        pane.fan_out("hawk/0", &snapshot_msg("hawk/0"));

        assert!(
            matches!(rx.try_recv(), Ok(ServerMessage::TerminalSnapshot { .. })),
            "a healthy viewer receives the frame"
        );
        assert_eq!(pane.viewers.len(), 1, "a healthy viewer is retained");
    }

    #[test]
    fn fan_out_sends_lagged_via_ctrl_and_drops_full_viewer() {
        // A capacity-1 data channel pre-filled so the next send overflows; the
        // viewer must then get a `Lagged` on its ctrl channel and be removed.
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = crate::fixtures::make_outbound();
        data_tx.try_send(snapshot_msg("hawk/0")).unwrap(); // fill to capacity

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(9),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: false,
                pause_auto: false,
                no_auto_pause: false,
            },
        );

        pane.fan_out("hawk/0", &snapshot_msg("hawk/0"));

        let lagged = ctrl_rx.try_recv().expect("Lagged must arrive on ctrl");
        assert!(
            matches!(&lagged, ServerMessage::Lagged { pane_id, .. } if pane_id == "hawk/0"),
            "a backed-up viewer is signalled Lagged out-of-band, got {lagged:?}",
        );
        assert!(
            pane.viewers.is_empty(),
            "a lagged viewer is dropped (it re-attaches and resyncs from the mirror)"
        );
    }

    #[test]
    fn fan_out_skips_paused_viewer_without_dropping_it() {
        // A paused viewer (issue #68) receives nothing and is retained even when its
        // channel is full — it resyncs on resume via re-attach, never lagged.
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = crate::fixtures::make_outbound();
        data_tx.try_send(snapshot_msg("hawk/0")).unwrap(); // fill to capacity

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(5),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: true,
                pause_auto: false,
                no_auto_pause: false,
            },
        );

        pane.fan_out("hawk/0", &snapshot_msg("hawk/0"));

        assert!(
            ctrl_rx.try_recv().is_err(),
            "a paused viewer must not be sent Lagged even with a full channel"
        );
        assert_eq!(
            pane.viewers.len(),
            1,
            "a paused viewer must be retained, not dropped"
        );
    }

    #[test]
    fn fan_out_streams_auto_pause_exempt_viewer() {
        // An auto-paused viewer with a per-pane exemption keeps streaming through
        // the background pause (issue #68); a manual pause would still skip it.
        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(8);
        let (ctrl_tx, _ctrl_rx) = crate::fixtures::make_outbound();

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(5),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: true,
                pause_auto: true,
                no_auto_pause: true,
            },
        );

        pane.fan_out("hawk/0", &snapshot_msg("hawk/0"));
        assert!(
            data_rx.try_recv().is_ok(),
            "an auto-pause-exempt viewer keeps receiving frames"
        );
    }

    #[test]
    fn fan_out_drops_closed_viewer_silently() {
        let (data_tx, data_rx) = mpsc::channel::<ServerMessage>(8);
        let (ctrl_tx, mut ctrl_rx) = crate::fixtures::make_outbound();
        drop(data_rx); // receiver gone → channel closed

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(3),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: false,
                pause_auto: false,
                no_auto_pause: false,
            },
        );

        pane.fan_out("hawk/0", &snapshot_msg("hawk/0"));

        assert!(
            pane.viewers.is_empty(),
            "a viewer whose channel closed is dropped"
        );
        assert!(
            ctrl_rx.try_recv().is_err(),
            "a closed viewer gets no Lagged — it is simply gone"
        );
    }

    #[test]
    fn is_federated_pane_reflects_word_index() {
        let mgr = PeerManager::new();
        assert!(!mgr.is_federated_pane("hawk/0"));
        mgr.word_index
            .lock()
            .unwrap()
            .insert("hawk".to_string(), "box:9000".to_string());
        assert!(mgr.is_federated_pane("hawk/0"));
        assert!(mgr.is_federated_pane("hawk/3"));
        assert!(!mgr.is_federated_pane("otherword/0"));
        assert!(!mgr.is_federated_pane("malformed"));
    }

    /// The SSH-federation success path parks the tunnel via `disarm`, which must
    /// hand the child back **alive** — killing it here would tear down the `-L`
    /// forward the freshly-opened peer depends on.
    #[cfg(unix)]
    #[tokio::test]
    async fn tunnel_guard_disarm_parks_the_child_alive() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let mut guard = TunnelGuard(Some(child));
        let mut parked = guard.disarm().expect("disarm yields the child");
        drop(guard); // Now a no-op: it must NOT kill the parked child.

        assert!(
            parked.try_wait().expect("try_wait").is_none(),
            "a disarmed tunnel must stay running for the live peer",
        );
        let _ = parked.start_kill();
        let _ = parked.wait().await;
    }

    /// Daemon shutdown must kill every peer's SSH tunnel synchronously (the
    /// runtime is torn down in the background, racing process exit, and
    /// `tokio::process::Child` is not kill-on-drop) and clear all peer state.
    #[cfg(unix)]
    #[tokio::test]
    async fn close_all_kills_tunnels_and_clears_peers() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let mgr = PeerManager::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut conn = PeerConnection::new(tx, no_reconnect());
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = Pid::from_raw(child.id().expect("child pid") as i32);
        conn.ssh_tunnel = Some(child);
        // A never-ending feed loop stand-in, so we exercise the abort path too.
        conn.feed_task = Some(tokio::spawn(std::future::pending::<()>()));

        let peer_id = "box:9000".to_string();
        mgr.peers
            .lock()
            .unwrap()
            .insert(peer_id.clone(), Arc::new(Mutex::new(conn)));
        mgr.word_index
            .lock()
            .unwrap()
            .insert("hawk".to_string(), peer_id);

        mgr.close_all();

        assert!(
            mgr.peers.lock().unwrap().is_empty(),
            "close_all must drop every peer"
        );
        assert!(
            mgr.word_index.lock().unwrap().is_empty(),
            "close_all must clear the word index"
        );

        // The tunnel pid must disappear (a live `sleep 60` would persist).
        let mut gone = false;
        for _ in 0..200 {
            if kill(pid, None).is_err() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "close_all must kill the SSH tunnel");
    }

    /// An un-disarmed guard (any error between `negotiate` and registration, or a
    /// later `close_peer`/`reap`) must kill the tunnel — `tokio::process::Child`
    /// is not kill-on-drop, so a leak here would orphan an `ssh -L` per failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn tunnel_guard_kills_on_drop() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let pid = Pid::from_raw(child.id().expect("child pid") as i32);
        drop(TunnelGuard(Some(child))); // Drop sends SIGKILL; tokio reaps the zombie.

        // A live `sleep 60` would keep existing; the kill makes the pid disappear.
        let mut gone = false;
        for _ in 0..200 {
            if kill(pid, None).is_err() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "a dropped TunnelGuard must kill the tunnel process");
    }

    /// Two opens of the same peer racing to publish: the first wins and keeps
    /// its link, words and tunnel; the second is torn down — its tunnel
    /// killed, its words never indexed.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_loser_of_a_concurrent_open_is_torn_down() {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let app = Arc::new(crate::fixtures::fixture_app());
        let peer_id = "box:9000".to_string();
        let publish = |local_word: &str| {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut conn = PeerConnection::new(tx, no_reconnect());
            let child = tokio::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn sleep");
            let pid =
                Pid::from_raw(i32::try_from(child.id().expect("child pid")).expect("pid fits"));
            conn.ssh_tunnel = Some(child);
            conn.register_session(
                local_word.to_string(),
                "remote".to_string(),
                sample_remote_entry("remote"),
                &peer_id,
            );
            let (_server_tx, server_rx) = mpsc::unbounded_channel();
            let won = app.peer_manager.publish_peer(
                &app,
                &peer_id,
                conn,
                server_rx,
                &[local_word.to_string()],
            );
            (won, pid)
        };
        let alive = |pid| kill(pid, None).is_ok();

        let hawk = app.draw_word().expect("a word");
        let owl = app.draw_word().expect("a word");
        let pool = app.available_words();
        let (won, winner) = publish(&hawk);
        let (lost, loser) = publish(&owl);
        assert!(won && !lost);
        assert_eq!(
            app.available_words(),
            pool + 1,
            "the loser's word is returned"
        );
        let index = app.peer_manager.word_index.lock().unwrap().clone();
        assert_eq!(index.get(&hawk), Some(&peer_id));
        assert!(!index.contains_key(&owl), "the loser's word is not indexed");
        let mut gone = false;
        for _ in 0..200 {
            if !alive(loser) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "the loser's tunnel is killed");
        assert!(alive(winner), "the winner's tunnel lives on");
        app.peer_manager.close_all();
    }

    /// A word still names a peer's session only while that peer is open and
    /// maps it to that same remote session (issue #208).
    #[tokio::test]
    async fn a_word_names_a_peer_session_only_while_both_hold() {
        let app = Arc::new(crate::fixtures::fixture_app());
        let (_upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");
        let mgr = &app.peer_manager;
        let conn = Arc::clone(&mgr.peers.lock().unwrap()["peer:1"]);
        assert!(mgr.still_names("fedlocal", &conn, "fedremote"));
        assert!(!mgr.still_names("fedlocal", &conn, "other"));
        assert!(!mgr.still_names("unknown", &conn, "fedremote"));
        app.close_peer("peer:1");
        assert_eq!(
            lock(&conn)
                .local_to_remote
                .get("fedlocal")
                .map(String::as_str),
            Some("fedremote"),
            "close_peer leaves the connection's maps"
        );
        assert!(!mgr.still_names("fedlocal", &conn, "fedremote"));
    }

    /// Opening a peer that is open already reuses it without dialling. A live
    /// one keeps its link as it is; an unreachable one is re-opened with the
    /// new target from then on — a restarted Direct peer's token rotates, and
    /// the token is not part of its id (issue #208).
    #[tokio::test]
    async fn reopening_an_unreachable_peer_retargets_its_link() {
        let app = Arc::new(crate::fixtures::fixture_app());
        let (_upstream, _peer) = app.peer_manager.install_channel_peer(
            &app,
            "box:9000",
            "fedlocal",
            "fedremote",
            no_reconnect(),
        );
        let conn = Arc::clone(&app.peer_manager.peers.lock().unwrap()["box:9000"]);
        let before = Arc::clone(&lock(&conn).connector);
        let target = PeerTarget::Direct {
            host: "box".to_string(),
            port: 9000,
            token: "rotated".to_string(),
            accept_invalid_certs: true,
        };

        let reopened = app.peer_manager.open_peer(&app, target.clone()).await;
        assert_eq!(reopened, Ok("box:9000".to_string()));
        assert!(Arc::ptr_eq(&before, &lock(&conn).connector), "live: kept");

        lock(&conn).dead = true;
        let reopened = app.peer_manager.open_peer(&app, target).await;
        assert_eq!(reopened, Ok("box:9000".to_string()));
        assert!(
            !Arc::ptr_eq(&before, &lock(&conn).connector),
            "unreachable: re-targeted"
        );
        assert_eq!(app.list_federated_sessions().len(), 1, "still one peer");
    }
}
