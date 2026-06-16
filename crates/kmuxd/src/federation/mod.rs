//! Daemon federation (issue #121): the local `kmuxd` opens an upstream
//! connection to a remote `kmuxd` and proxies the remote's sessions to local
//! GUIs, so N windows on a remote host cost **one** network connection instead
//! of N. See `docs/architecture-federation.md`.
//!
//! # Model
//!
//! [`PeerManager`] (held on [`ServerApp`](crate::app::ServerApp)) owns one
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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kmux_client::grid::CellGrid;
use kmux_connect::connect::ConnectResult;
use kmux_connect::ssh::{self, RemoteTarget};
use kmux_connect::tcp_connect::connect_tcp_tls;
use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, PeerId, PeerTarget, RequestId, SequenceNo,
    ServerMessage, SessionEntry, SessionEventMsg, TermSize, epoch_millis,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::app::ServerApp;

/// How long [`PeerManager::open_peer`] waits for the upstream `AuthResult`.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long [`PeerManager::open_peer`] waits for the upstream session list.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long [`PeerManager::create_remote_session`] waits for the upstream
/// `SessionCreated` (or `Error`) when creating a session on a federated peer.
const CREATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Owns every upstream peer connection and routes federated traffic.
#[derive(Default)]
pub struct PeerManager {
    /// Open peers keyed by their stable [`PeerId`].
    peers: Mutex<HashMap<PeerId, Arc<Mutex<PeerConnection>>>>,
    /// `local_word -> PeerId`, so the dispatch layer can resolve a federated
    /// pane to its owning peer with a single lookup.
    word_index: Mutex<HashMap<String, PeerId>>,
}

/// One local GUI viewing a proxied pane: its bounded data channel, its unbounded
/// ctrl channel (out-of-band signalling that bypasses data backpressure, e.g.
/// `Lagged`), and its declared terminal size (used for smallest-wins upstream
/// reconciliation).
struct Viewer {
    data_tx: mpsc::Sender<ServerMessage>,
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    size: TermSize,
    /// Connection-pause state (issue #68). While paused this viewer receives no
    /// terminal-output frames (it is skipped in `fan_out`, never marked lagged) and
    /// catches up on resume via re-attach; it still counts toward `effective_size`,
    /// exactly as a paused client does in the local PTY relay.
    paused: bool,
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

    /// The viewers' **unbounded ctrl** senders — the delivery path for session
    /// events and lifecycle signals, which must not be subject to the per-pane data
    /// backpressure (matching the local daemon, where events reach clients via the
    /// unbounded writer channel, not the bounded pane stream).
    fn viewer_ctrl_senders(&self) -> Vec<mpsc::UnboundedSender<ServerMessage>> {
        self.viewers.values().map(|v| v.ctrl_tx.clone()).collect()
    }

    /// Fan an (already local-addressed) pane frame out to every viewer, applying
    /// the same backpressure policy as the local PTY relay
    /// ([`crate::relay::broadcast_to_clients`]): a viewer whose **bounded** data
    /// channel is full is sent a [`ServerMessage::Lagged`] over its **unbounded**
    /// ctrl channel and dropped — it re-attaches and is served a fresh snapshot
    /// minted off the still-correct mirror, exactly as a lagging local client
    /// recovers. A viewer whose channel has closed is dropped silently. The mirror
    /// is fed by the caller *before* this, so a dropped viewer never desyncs it.
    fn fan_out(&mut self, local_pane_id: &str, msg: &ServerMessage) {
        let mut dead: Vec<ClientId> = Vec::new();
        for (&client_id, viewer) in self.viewers.iter() {
            // Paused viewers (issue #68) receive no terminal output and must never
            // be marked lagged or dropped when their channel fills — they resync on
            // resume via re-attach. Same rule as `relay::broadcast_to_clients`.
            if viewer.paused {
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
                self.mirror.apply_snapshot(snapshot.clone());
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
    /// The background `ssh -L -N` tunnel process for an [`PeerTarget::Ssh`] peer,
    /// kept alive for the life of the connection (the `-L` forward dies with it).
    /// `None` for a [`PeerTarget::Direct`] peer. Killed on close/reap.
    ssh_tunnel: Option<tokio::process::Child>,
    /// Set by the feed loop when the upstream link closes. A dead connection is
    /// reaped lazily on the next `open_peer` to the same peer (which holds the
    /// `&ServerApp` needed to release the local words), so re-federation works.
    dead: bool,
}

impl PeerConnection {
    fn new(client_tx: mpsc::UnboundedSender<ClientMessage>) -> Self {
        Self {
            client_tx,
            remote_to_local: HashMap::new(),
            local_to_remote: HashMap::new(),
            sessions: HashMap::new(),
            panes: HashMap::new(),
            feed_task: None,
            next_request_id: 2,
            pending_creates: HashMap::new(),
            ssh_tunnel: None,
            dead: false,
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
        let (remote_word, idx) = split_pane_id(remote_pane)?;
        let local_word = self.remote_to_local.get(remote_word)?;
        Some(format!("{local_word}/{idx}"))
    }

    /// The **ctrl** senders of every viewer of every proxied pane under
    /// `local_word` — the routing target for session-scoped events (titles, layout,
    /// lifecycle), which in local `kmuxd` reach all clients viewing a session, not
    /// just one pane, and are delivered out-of-band (unbounded) so backpressure on a
    /// pane's content stream can never drop a title change or a `SessionClosed`.
    fn viewers_under_word(&self, local_word: &str) -> Vec<mpsc::UnboundedSender<ServerMessage>> {
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

/// A resolved TCP+TLS endpoint for an upstream peer link. Both [`PeerTarget`]
/// variants reduce to this: `Direct` is the endpoint verbatim; `Ssh` is the
/// loopback end of an `-L` tunnel (with the tunnel child retained so it outlives
/// the connection). From here the connect/auth/list/register path is identical.
struct PeerConnectPlan {
    host: String,
    port: u16,
    /// TOFU identity for cert pinning — for SSH this is the *real* remote
    /// `host:tcp_port`, not the ephemeral loopback the tunnel listens on.
    tofu_key: String,
    token: String,
    accept_invalid: bool,
    ssh_tunnel: Option<tokio::process::Child>,
}

/// Kills a parked SSH `-L` tunnel on drop unless [`disarm`](Self::disarm)ed.
/// `tokio::process::Child` is not kill-on-drop, so any error between
/// `ssh::negotiate` and a fully-registered peer would otherwise leak the
/// process. Disarmed once the tunnel is parked on the live [`PeerConnection`].
struct TunnelGuard(Option<tokio::process::Child>);

impl TunnelGuard {
    /// Take the child out, disarming the guard (the caller owns teardown now).
    fn disarm(&mut self) -> Option<tokio::process::Child> {
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

    /// Ensure an upstream connection to `target` exists and surface its sessions
    /// locally, returning the peer's [`PeerId`]. Idempotent: an already-open peer
    /// is reused. Performs the connect + auth + session-list handshake inline, so
    /// the caller awaits a fully-registered peer before replying `PeerOpened`.
    pub async fn open_peer(&self, app: &ServerApp, target: PeerTarget) -> Result<PeerId, String> {
        let peer_id = target.peer_id();

        // Reuse a live peer; reap a dead one (its upstream closed) so this becomes
        // a fresh federation rather than handing back a defunct connection.
        let existing_dead = self
            .peers
            .lock()
            .unwrap()
            .get(&peer_id)
            .map(|c| c.lock().unwrap().dead);
        match existing_dead {
            Some(false) => {
                debug!(%peer_id, "reusing existing peer connection");
                return Ok(peer_id);
            }
            Some(true) => self.reap_dead_peer(app, &peer_id),
            None => {}
        }

        // Resolve the target to a TCP+TLS endpoint. A `Direct` peer is that
        // endpoint verbatim; an `Ssh` peer first negotiates a `-L` tunnel
        // (`kmuxd probe-or-start` over SSH, then forward a loopback port) and is
        // reached over TCP+TLS through it — identical from here on. The tunnel
        // child is carried in the plan so it can be parked on the connection.
        let plan = match target {
            PeerTarget::Direct {
                host,
                port,
                token,
                accept_invalid_certs,
            } => PeerConnectPlan {
                tofu_key: format!("{host}:{port}"),
                host,
                port,
                token,
                accept_invalid: accept_invalid_certs,
                ssh_tunnel: None,
            },
            PeerTarget::Ssh {
                user,
                host,
                ssh_port,
                accept_invalid_certs,
            } => {
                let remote = RemoteTarget {
                    user,
                    host,
                    ssh_port,
                };
                let ssh = ssh::negotiate(&remote)
                    .await
                    .map_err(|e| format!("SSH peer negotiation failed: {e}"))?;
                PeerConnectPlan {
                    tofu_key: format!("{}:{}", ssh.remote_host, ssh.remote_tcp_port),
                    host: "127.0.0.1".to_string(),
                    port: ssh.local_tcp_port,
                    token: ssh.token,
                    accept_invalid: accept_invalid_certs,
                    ssh_tunnel: Some(ssh.tunnel_process),
                }
            }
        };

        let PeerConnectPlan {
            host,
            port,
            tofu_key,
            token,
            accept_invalid,
            ssh_tunnel,
        } = plan;
        // Hold the SSH tunnel in a kill-on-drop guard so any early-return error
        // path below (connect, auth, or session-list failure/timeout) tears down
        // the `ssh -L` process — `tokio::process::Child` is not kill-on-drop. The
        // guard is disarmed at step 4 once the tunnel is parked on the connection.
        let mut tunnel = TunnelGuard(ssh_tunnel);

        // 1. Open the upstream link. `connect_tcp_tls` sends `Auth` itself and
        //    forwards every `ServerMessage` (incl. `AuthResult`) to `server_tx`.
        let (server_tx, mut server_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let client_tx = match connect_tcp_tls(
            host,
            port,
            tofu_key,
            token,
            server_tx,
            ClientCapabilities::default(),
            None,
            accept_invalid,
        )
        .await
        {
            ConnectResult::Connected(tx) => tx,
            ConnectResult::Failed(e) => return Err(format!("peer connect failed: {e}")),
        };

        // 2. Await authentication.
        match recv_until(&mut server_rx, AUTH_TIMEOUT, |m| {
            matches!(m, ServerMessage::AuthResult { .. })
        })
        .await
        {
            Some(ServerMessage::AuthResult { success: true, .. }) => {}
            Some(ServerMessage::AuthResult {
                success: false,
                reason,
                ..
            }) => {
                return Err(format!(
                    "peer rejected authentication: {}",
                    reason.unwrap_or_else(|| "unknown reason".to_string())
                ));
            }
            _ => return Err("peer did not complete authentication in time".to_string()),
        }

        // 3. Fetch the remote session list.
        if client_tx
            .send(ClientMessage::SessionList { request_id: 1 })
            .is_err()
        {
            return Err("peer connection closed before session list".to_string());
        }
        let remote_sessions = match recv_until(&mut server_rx, LIST_TIMEOUT, |m| {
            matches!(m, ServerMessage::SessionListResult { .. })
        })
        .await
        {
            Some(ServerMessage::SessionListResult { sessions, .. }) => sessions,
            _ => return Err("peer did not return a session list in time".to_string()),
        };

        // 4. Register each remote session under a fresh local word. Park the SSH
        //    tunnel (if any) on the connection — disarming the guard — so it lives
        //    as long as the link and is killed by `close_peer`/`reap_dead_peer`.
        let mut conn = PeerConnection::new(client_tx.clone());
        conn.ssh_tunnel = tunnel.disarm();
        let mut assigned_words: Vec<String> = Vec::new();
        for entry in remote_sessions {
            let remote_word = entry.meta.word_id.clone();
            let local_word = match app.draw_word() {
                Some(w) => w,
                None => {
                    for w in &assigned_words {
                        app.release_word(w);
                    }
                    return Err("local session word pool exhausted".to_string());
                }
            };
            assigned_words.push(local_word.clone());
            conn.register_session(local_word, remote_word, entry, &peer_id);
        }

        // 5. Publish the peer. The reuse check at the top of `open_peer` is not
        //    atomic with this insert across the `await`-heavy connect above, so two
        //    GUIs federating the *same* target concurrently can both reach here.
        //    Decide the winner under a single `peers` lock — the winner spawns its
        //    feed loop and inserts; a loser tears its duplicate down (closing the
        //    redundant upstream link + SSH tunnel and releasing its drawn words) and
        //    reuses the winner — so a race can never leak a connection or corrupt the
        //    word index. The word index is published only by the winner, while still
        //    holding `peers`, so a lookup that sees a word also finds its connection.
        let conn = Arc::new(Mutex::new(conn));
        let won = {
            let mut peers = self.peers.lock().unwrap();
            let taken = peers
                .get(&peer_id)
                .is_some_and(|existing| !existing.lock().unwrap().dead);
            if taken {
                false
            } else {
                let feed =
                    spawn_feed_loop(server_rx, client_tx, Arc::clone(&conn), peer_id.clone());
                conn.lock().unwrap().feed_task = Some(feed);
                peers.insert(peer_id.clone(), Arc::clone(&conn));
                let mut idx = self.word_index.lock().unwrap();
                for w in &assigned_words {
                    idx.insert(w.clone(), peer_id.clone());
                }
                true
            }
        };

        if !won {
            // Lost the race: tear down this duplicate (no feed loop was spawned, so
            // dropping `conn` closes its `client_tx` and the upstream link) and
            // return the drawn words to the pool.
            if let Some(mut child) = conn.lock().unwrap().ssh_tunnel.take() {
                let _ = child.start_kill();
            }
            for w in &assigned_words {
                app.release_word(w);
            }
            debug!(%peer_id, "lost concurrent open race; discarded duplicate peer link");
            return Ok(peer_id);
        }

        info!(%peer_id, sessions = assigned_words.len(), "federated peer opened");
        Ok(peer_id)
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
        // the new session is addressable and its panes route as federated.
        let remote_word = remote_entry.meta.word_id.clone();
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
    pub fn close_peer(&self, app: &ServerApp, peer_id: &str) {
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
        }
        info!(%peer_id, "federated peer closed");
    }

    /// Remove a peer whose upstream link already died (feed loop set `dead`),
    /// releasing its local words back to the pool and clearing the word index so a
    /// fresh `open_peer` to the same address starts clean.
    fn reap_dead_peer(&self, app: &ServerApp, peer_id: &str) {
        let conn = self.peers.lock().unwrap().remove(peer_id);
        let Some(conn) = conn else { return };
        let mut guard = conn.lock().unwrap();
        if let Some(mut child) = guard.ssh_tunnel.take() {
            let _ = child.start_kill();
        }
        let mut idx = self.word_index.lock().unwrap();
        for local_word in guard.local_to_remote.keys() {
            idx.remove(local_word);
            app.release_word(local_word);
        }
        debug!(%peer_id, "reaped dead peer before re-federation");
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
        match split_pane_id(pane_id) {
            Some((word, _)) => self.word_index.lock().unwrap().contains_key(word),
            None => false,
        }
    }

    /// Proxied sessions across all peers (local IDs, peer-decorated names).
    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        let peers = self.peers.lock().unwrap();
        let mut out = Vec::new();
        for conn in peers.values() {
            out.extend(conn.lock().unwrap().sessions.values().cloned());
        }
        out
    }

    /// Translate `local_pane_id` to its remote form and forward
    /// `build(remote_pane_id)` upstream. Returns `false` (forwarding nothing) when
    /// the pane is not federated, so the caller can fall back to local handling.
    pub fn forward_message(
        &self,
        local_pane_id: &str,
        build: impl FnOnce(String) -> ClientMessage,
    ) -> bool {
        let Some((local_word, idx)) = split_pane_id(local_pane_id) else {
            return false;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return false;
        };
        let guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word) else {
            return false;
        };
        let remote_pane = format!("{remote_word}/{idx}");
        guard.client_tx.send(build(remote_pane)).is_ok()
    }

    /// Register `data_tx` as a viewer of federated `local_pane_id`. The **first**
    /// viewer of a pane forwards an `Attach` upstream (the remote streams a
    /// snapshot back); a **later** viewer is served a snapshot minted from the live
    /// mirror with **no upstream round-trip**, then the upstream size is reconciled
    /// (smallest-wins). Returns `false` when the pane is not federated.
    pub fn attach_viewer(
        &self,
        local_pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        last_seqno: Option<SequenceNo>,
        size: TermSize,
    ) -> bool {
        let Some((local_word, idx)) = split_pane_id(local_pane_id) else {
            return false;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return false;
        };
        let mut guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return false;
        };
        let remote_pane = format!("{remote_word}/{idx}");

        if guard.panes.contains_key(local_pane_id) {
            // Late viewer: mint from the mirror, register, then reconcile size.
            let pane = guard.panes.get_mut(local_pane_id).unwrap();
            let minted = ServerMessage::TerminalSnapshot {
                pane_id: local_pane_id.to_string(),
                snapshot: pane.mirror.to_snapshot(),
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
                },
            );
            guard.panes.insert(local_pane_id.to_string(), pane);
            let _ = guard.client_tx.send(ClientMessage::Attach {
                pane_id: remote_pane,
                last_seqno,
                size,
            });
        }
        true
    }

    /// Update `client_id`'s declared size for federated `local_pane_id` and
    /// reconcile the smallest-wins size upstream. Returns `false` when the pane is
    /// not federated.
    pub fn resize_viewer(&self, local_pane_id: &str, client_id: ClientId, size: TermSize) -> bool {
        let Some((local_word, idx)) = split_pane_id(local_pane_id) else {
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
        let remote_pane = format!("{remote_word}/{idx}");
        guard.reconcile_size(local_pane_id, &remote_pane);
        true
    }

    /// Remove `client_id` as a viewer of federated `local_pane_id`. When it was the
    /// **last** viewer, forward a `Detach` upstream and drop the mirror; otherwise
    /// reconcile the upstream size (a departing viewer may have been the smallest).
    pub fn detach_viewer(&self, local_pane_id: &str, client_id: ClientId) {
        let Some((local_word, idx)) = split_pane_id(local_pane_id) else {
            return;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return;
        };
        let mut guard = conn.lock().unwrap();
        let Some(remote_word) = guard.local_to_remote.get(local_word).cloned() else {
            return;
        };
        let remote_pane = format!("{remote_word}/{idx}");
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
    pub fn set_paused(&self, client_id: ClientId, paused: bool) {
        let conns: Vec<_> = self.peers.lock().unwrap().values().cloned().collect();
        for conn in conns {
            let mut guard = conn.lock().unwrap();
            for pane in guard.panes.values_mut() {
                if let Some(viewer) = pane.viewers.get_mut(&client_id) {
                    viewer.paused = paused;
                }
            }
        }
    }

    fn conn_for_word(&self, local_word: &str) -> Option<Arc<Mutex<PeerConnection>>> {
        let peer_id = self.word_index.lock().unwrap().get(local_word).cloned()?;
        self.peers.lock().unwrap().get(&peer_id).cloned()
    }
}

/// Drain the upstream `ServerMessage` stream: answer keepalive pings, translate
/// pane IDs from remote to local, and fan each pane frame out to its viewers.
fn spawn_feed_loop(
    mut server_rx: mpsc::UnboundedReceiver<ServerMessage>,
    client_tx: mpsc::UnboundedSender<ClientMessage>,
    conn: Arc<Mutex<PeerConnection>>,
    peer_id: PeerId,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(mut msg) = server_rx.recv().await {
            // Answer keepalive pings so the upstream considers us live.
            if let ServerMessage::Ping { seq } = msg {
                let _ = client_tx.send(ClientMessage::Pong { seq });
                continue;
            }

            // Route responses to hub-initiated requests (create-on-peer) back to
            // the waiting oneshot. `SessionCreated` carries the *remote* entry;
            // `create_remote_session` (holding `&ServerApp`) draws the local word
            // and registers it. An `Error`/`SessionCreated` whose id we are not
            // waiting on falls through to the normal handling below.
            let response_rid = match &msg {
                ServerMessage::SessionCreated { request_id, .. } => Some(*request_id),
                ServerMessage::Error {
                    request_id: Some(rid),
                    ..
                } => Some(*rid),
                _ => None,
            };
            if let Some(rid) = response_rid
                && let Some(tx) = conn.lock().unwrap().pending_creates.remove(&rid)
            {
                let result = match msg {
                    ServerMessage::SessionCreated { entry, .. } => Ok(entry),
                    ServerMessage::Error { message, .. } => Err(message),
                    _ => unreachable!("response_rid is set only for those two variants"),
                };
                let _ = tx.send(result);
                continue;
            }

            // Pane-scoped frames: translate the pane ID remote→local, feed the
            // pane's mirror (so a late local attacher can be served from it), then
            // fan out to that pane's viewers. `fan_out` applies the relay's
            // backpressure policy: a full viewer is sent `Lagged` (out-of-band) and
            // dropped, then resyncs on re-attach from the just-updated mirror.
            if let Some(remote_pane) = msg_pane_id(&msg).map(str::to_string) {
                let mut guard = conn.lock().unwrap();
                if let Some(local_pane) = guard.to_local_pane(&remote_pane) {
                    set_msg_pane_id(&mut msg, local_pane.clone());
                    if let Some(pane) = guard.panes.get_mut(&local_pane) {
                        pane.apply_to_mirror(&msg);
                        pane.fan_out(&local_pane, &msg);
                    }
                }
                continue;
            }

            // Session-scoped events (titles, layout, tab/session lifecycle): translate
            // the embedded word remote→local and fan out to every viewer under that
            // word, so a GUI viewing a federated session still receives its title and
            // layout updates. These go over the viewers' unbounded ctrl channel
            // (`viewers_under_word`), so — exactly as in the local daemon — a backed-up
            // pane content stream can never drop an event.
            let viewers = {
                let guard = conn.lock().unwrap();
                match &mut msg {
                    ServerMessage::Event { event } => {
                        rewrite_event_to_local(event, &guard.remote_to_local)
                            .map(|local_word| guard.viewers_under_word(&local_word))
                    }
                    ServerMessage::LayoutUpdate { word_id, .. } => guard
                        .remote_to_local
                        .get(word_id.as_str())
                        .cloned()
                        .map(|local_word| {
                            *word_id = local_word.clone();
                            guard.viewers_under_word(&local_word)
                        }),
                    _ => None,
                }
            };
            if let Some(viewers) = viewers {
                for tx in viewers {
                    let _ = tx.send(msg.clone());
                }
            }
        }

        // The upstream link closed (remote daemon gone, network dropped). Isolate
        // the failure: tell every viewer its proxied session ended so the GUI
        // cleans up instead of hanging, drop the panes (closing the pane streams),
        // and mark the connection dead for lazy reaping. Locally-hosted PTY panes
        // are untouched — they live in a separate relay. The `SessionClosed` goes
        // over the unbounded ctrl channel so a viewer whose data stream happened to
        // be full at death still learns the session ended (a bounded `try_send`
        // could drop it and the GUI would hang, since no further frames follow).
        {
            let mut guard = conn.lock().unwrap();
            guard.dead = true;
            let words: Vec<String> = guard.sessions.keys().cloned().collect();
            for local_word in &words {
                let closed = ServerMessage::Event {
                    event: SessionEventMsg::SessionClosed {
                        word_id: local_word.clone(),
                    },
                };
                for tx in guard.viewers_under_word(local_word) {
                    let _ = tx.send(closed.clone());
                }
            }
            // Drop panes (closing pane streams) and sessions (so a post-death
            // `SessionList` no longer lists this peer's now-gone sessions).
            guard.panes.clear();
            guard.sessions.clear();
        }
        debug!(%peer_id, "federation feed loop ended (upstream closed); viewers notified");
    })
}

/// Rewrite a remote [`SessionEntry`] into its local form: a freshly-assigned
/// word, local pane IDs, a peer-decorated display name, and cleared
/// `attached_clients` (the remote's client IDs are meaningless locally).
fn localize_entry(mut entry: SessionEntry, local_word: &str, peer_id: &str) -> SessionEntry {
    entry.meta.name = format!("{} @ {peer_id}", entry.meta.name);
    entry.meta.word_id = local_word.to_string();
    // Attribute the session to its peer so clients can group it by machine. The
    // name decoration above stays for now (older/CLI views still rely on it); a
    // frontend that groups by `peer` strips the decoration for display.
    entry.peer = Some(peer_id.to_string());
    for pane in &mut entry.panes {
        pane.pane_id = format!("{local_word}/{}", pane.pane_index);
        pane.attached_clients.clear();
    }
    entry
}

/// Rewrite the word (or pane) a [`SessionEventMsg`] references from remote to
/// local, returning the local word for routing. `None` when the referenced word
/// is not federated (e.g. an event for a remote session we never registered).
fn rewrite_event_to_local(
    event: &mut SessionEventMsg,
    remote_to_local: &std::collections::HashMap<String, String>,
) -> Option<String> {
    use SessionEventMsg::*;
    match event {
        PaneSpawned { pane_id }
        | PaneExited { pane_id, .. }
        | PaneResized { pane_id, .. }
        | PaneTitleChanged { pane_id, .. }
        | PaneClipboardCopy { pane_id, .. }
        | PaneClosed { pane_id } => {
            let (remote_word, idx) = split_pane_id(pane_id)?;
            let local_word = remote_to_local.get(remote_word)?.clone();
            *pane_id = format!("{local_word}/{idx}");
            Some(local_word)
        }
        SessionCreated { word_id }
        | SessionClosed { word_id }
        | SessionRenamed { word_id, .. }
        | TabCreated { word_id, .. }
        | TabClosed { word_id, .. }
        | TabRenamed { word_id, .. }
        | LayoutChanged { word_id, .. } => {
            let local_word = remote_to_local.get(word_id.as_str())?.clone();
            *word_id = local_word.clone();
            Some(local_word)
        }
    }
}

/// Split a `"word/index"` pane ID into its parts. Returns `None` for a malformed
/// ID or an empty word.
fn split_pane_id(pane_id: &str) -> Option<(&str, u32)> {
    let (word, idx) = pane_id.rsplit_once('/')?;
    if word.is_empty() {
        return None;
    }
    Some((word, idx.parse().ok()?))
}

/// Borrow the single `pane_id` a [`ServerMessage`] carries, if any.
fn msg_pane_id(msg: &ServerMessage) -> Option<&str> {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. }
        | PaneCreated { pane_id, .. }
        | PaneClosed { pane_id, .. }
        | HistoryLines { pane_id, .. }
        | InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => Some(pane_id.as_str()),
        _ => None,
    }
}

/// Overwrite the `pane_id` a [`ServerMessage`] carries (no-op if it has none).
fn set_msg_pane_id(msg: &mut ServerMessage, new_id: String) {
    use ServerMessage::*;
    match msg {
        TerminalUpdate { pane_id, .. }
        | TerminalSnapshot { pane_id, .. }
        | CursorUpdate { pane_id, .. }
        | ScrollbackAppend { pane_id, .. }
        | SyncReset { pane_id }
        | Lagged { pane_id, .. }
        | PaneCreated { pane_id, .. }
        | PaneClosed { pane_id, .. }
        | HistoryLines { pane_id, .. }
        | InputLockGranted { pane_id }
        | InputLockDenied { pane_id, .. }
        | InputLockReleased { pane_id } => *pane_id = new_id,
        _ => warn!("set_msg_pane_id called on a message with no pane_id"),
    }
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
    use super::*;
    use kmux_protocol::messages::{
        GridSnapshot, PaneInfo, SequenceNo, SessionMeta, SessionStatus, TabInfo,
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
                pane_id: format!("{word}/0"),
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
            }],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".to_string(),
                layout: kmux_protocol::messages::LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        }
    }

    fn snapshot_msg(pane_id: &str) -> ServerMessage {
        ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot: GridSnapshot {
                rows: 1,
                cols: 1,
                cells: vec![],
                cursor: Default::default(),
                modes: kmux_protocol::messages::TermModes::EMPTY,
                history_total: 0,
                scrollback_base: 0,
                scrollback_tail: vec![],
            },
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        }
    }

    #[test]
    fn split_pane_id_parses_and_rejects() {
        assert_eq!(split_pane_id("eagle/0"), Some(("eagle", 0)));
        assert_eq!(split_pane_id("two/words/3"), Some(("two/words", 3)));
        assert_eq!(split_pane_id("noindex"), None);
        assert_eq!(split_pane_id("/0"), None);
        assert_eq!(split_pane_id("eagle/x"), None);
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
        let mut map = std::collections::HashMap::new();
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
        let mut conn = PeerConnection::new(tx);
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
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel();
        (
            Viewer {
                data_tx,
                ctrl_tx,
                size,
                paused: false,
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
            snapshot: GridSnapshot {
                rows: 1,
                cols: 3,
                cells,
                cursor: Default::default(),
                modes: kmux_protocol::messages::TermModes::EMPTY,
                history_total: 0,
                scrollback_base: 0,
                scrollback_tail: vec![],
            },
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
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        data_tx.try_send(snapshot_msg("hawk/0")).unwrap(); // fill to capacity

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(9),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: false,
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
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        data_tx.try_send(snapshot_msg("hawk/0")).unwrap(); // fill to capacity

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(5),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: true,
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
    fn fan_out_drops_closed_viewer_silently() {
        let (data_tx, data_rx) = mpsc::channel::<ServerMessage>(8);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        drop(data_rx); // receiver gone → channel closed

        let mut pane = ProxiedPane::new(sz(24, 80));
        pane.viewers.insert(
            ClientId(3),
            Viewer {
                data_tx,
                ctrl_tx,
                size: sz(24, 80),
                paused: false,
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
        let mut conn = PeerConnection::new(tx);
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
}
