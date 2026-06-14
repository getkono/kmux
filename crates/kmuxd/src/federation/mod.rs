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
//! local IDs), and the set of local viewers per proxied pane. A per-peer **feed
//! loop** drains the upstream `ServerMessage` stream, rewrites each frame's pane
//! ID from remote to local, and fans it out to that pane's local viewers.
//!
//! Federated sessions are kept **entirely separate** from `ServerApp.sessions`
//! (which is strictly PTY-backed): a proxied pane has no local PTY, `term_state`
//! or scrollback, so it must never flow through the PTY relay machinery. The
//! daemon translates IDs at the dispatch boundary and forwards everything else
//! verbatim — the remote daemon needs no awareness of federation and sees the
//! local daemon as one ordinary client.
//!
//! This is the PR3 scope: **one** local viewer per proxied pane (a pure
//! message-translating proxy). Multi-viewer reconciliation — size-min, pause
//! union, capability merge, input-lock arbitration, and minting snapshots for
//! late local attachers from a cached `CellGrid` mirror — is PR4.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kmux_connect::connect::ConnectResult;
use kmux_connect::tcp_connect::connect_tcp_tls;
use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, PeerId, PeerTarget, SequenceNo, ServerMessage,
    SessionEntry, TermSize,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::app::ServerApp;

/// How long [`PeerManager::open_peer`] waits for the upstream `AuthResult`.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long [`PeerManager::open_peer`] waits for the upstream session list.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Owns every upstream peer connection and routes federated traffic.
#[derive(Default)]
pub struct PeerManager {
    /// Open peers keyed by their stable [`PeerId`].
    peers: Mutex<HashMap<PeerId, Arc<Mutex<PeerConnection>>>>,
    /// `local_word -> PeerId`, so the dispatch layer can resolve a federated
    /// pane to its owning peer with a single lookup.
    word_index: Mutex<HashMap<String, PeerId>>,
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
    /// Local viewers of each proxied pane: `local_pane_id -> {client_id -> data_tx}`.
    viewers: HashMap<String, HashMap<ClientId, mpsc::Sender<ServerMessage>>>,
    /// The feed-loop task draining the upstream stream; aborted on close.
    feed_task: Option<JoinHandle<()>>,
}

impl PeerConnection {
    fn new(client_tx: mpsc::UnboundedSender<ClientMessage>) -> Self {
        Self {
            client_tx,
            remote_to_local: HashMap::new(),
            local_to_remote: HashMap::new(),
            sessions: HashMap::new(),
            viewers: HashMap::new(),
            feed_task: None,
        }
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

    fn add_viewer(
        &mut self,
        local_pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
    ) {
        self.viewers
            .entry(local_pane_id.to_string())
            .or_default()
            .insert(client_id, data_tx);
    }

    /// Remove a viewer; returns `true` if the pane now has no viewers left.
    fn remove_viewer(&mut self, local_pane_id: &str, client_id: ClientId) -> bool {
        if let Some(vs) = self.viewers.get_mut(local_pane_id) {
            vs.remove(&client_id);
            if vs.is_empty() {
                self.viewers.remove(local_pane_id);
                return true;
            }
        }
        false
    }

    fn viewers_of(&self, local_pane_id: &str) -> Vec<mpsc::Sender<ServerMessage>> {
        self.viewers
            .get(local_pane_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
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

        if self.peers.lock().unwrap().contains_key(&peer_id) {
            debug!(%peer_id, "reusing existing peer connection");
            return Ok(peer_id);
        }

        // PR3 supports only the direct TCP+TLS endpoint (LAN / same-host / tests).
        // SSH peer setup (probe-or-start + `-L` tunnel) is deferred.
        let (host, port, token, accept_invalid) = match target {
            PeerTarget::Direct {
                host,
                port,
                token,
                accept_invalid_certs,
            } => (host, port, token, accept_invalid_certs),
            PeerTarget::Ssh { .. } => {
                return Err(
                    "SSH peer targets are not supported yet; use a direct TCP+TLS endpoint"
                        .to_string(),
                );
            }
        };

        // 1. Open the upstream link. `connect_tcp_tls` sends `Auth` itself and
        //    forwards every `ServerMessage` (incl. `AuthResult`) to `server_tx`.
        let (server_tx, mut server_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let tofu_key = format!("{host}:{port}");
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

        // 4. Register each remote session under a fresh local word.
        let mut conn = PeerConnection::new(client_tx.clone());
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

        // 5. Start the feed loop, then publish the peer + word index. Publishing
        //    last means a lookup that sees the word in `word_index` also finds the
        //    fully-built connection.
        let conn = Arc::new(Mutex::new(conn));
        let feed = spawn_feed_loop(server_rx, client_tx, Arc::clone(&conn), peer_id.clone());
        conn.lock().unwrap().feed_task = Some(feed);
        self.peers
            .lock()
            .unwrap()
            .insert(peer_id.clone(), Arc::clone(&conn));
        {
            let mut idx = self.word_index.lock().unwrap();
            for w in &assigned_words {
                idx.insert(w.clone(), peer_id.clone());
            }
        }

        info!(%peer_id, sessions = assigned_words.len(), "federated peer opened");
        Ok(peer_id)
    }

    /// Tear down the upstream connection to `peer_id`, release its local words,
    /// and abort its feed loop. No-op when the peer is unknown.
    pub fn close_peer(&self, app: &ServerApp, peer_id: &str) {
        let conn = self.peers.lock().unwrap().remove(peer_id);
        let Some(conn) = conn else { return };
        let guard = conn.lock().unwrap();
        if let Some(task) = &guard.feed_task {
            task.abort();
        }
        let mut idx = self.word_index.lock().unwrap();
        for local_word in guard.local_to_remote.keys() {
            idx.remove(local_word);
            app.release_word(local_word);
        }
        info!(%peer_id, "federated peer closed");
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

    /// Register `data_tx` as a viewer of federated `local_pane_id` and forward an
    /// `Attach` upstream. Returns `false` when the pane is not federated.
    pub fn attach_viewer(
        &self,
        local_pane_id: &str,
        client_id: ClientId,
        data_tx: mpsc::Sender<ServerMessage>,
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
        guard.add_viewer(local_pane_id, client_id, data_tx);
        let remote_pane = format!("{remote_word}/{idx}");
        let _ = guard.client_tx.send(ClientMessage::Attach {
            pane_id: remote_pane,
            last_seqno,
            size,
        });
        true
    }

    /// Remove `client_id` as a viewer of federated `local_pane_id`; when it was
    /// the last viewer, forward a `Detach` upstream so the remote stops streaming.
    pub fn detach_viewer(&self, local_pane_id: &str, client_id: ClientId) {
        let Some((local_word, idx)) = split_pane_id(local_pane_id) else {
            return;
        };
        let Some(conn) = self.conn_for_word(local_word) else {
            return;
        };
        let mut guard = conn.lock().unwrap();
        if guard.remove_viewer(local_pane_id, client_id)
            && let Some(remote_word) = guard.local_to_remote.get(local_word).cloned()
        {
            let remote_pane = format!("{remote_word}/{idx}");
            let _ = guard.client_tx.send(ClientMessage::Detach {
                pane_id: remote_pane,
            });
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

            // Only pane-scoped frames are proxied in PR3. Session-scoped events
            // (titles, layout, lifecycle) are forwarded in PR4.
            let Some(remote_pane) = msg_pane_id(&msg).map(str::to_string) else {
                continue;
            };

            let viewers = {
                let guard = conn.lock().unwrap();
                match guard.to_local_pane(&remote_pane) {
                    Some(local_pane) => {
                        set_msg_pane_id(&mut msg, local_pane.clone());
                        guard.viewers_of(&local_pane)
                    }
                    None => Vec::new(),
                }
            };
            for tx in viewers {
                // Best-effort: a full viewer channel drops this frame; the client
                // recovers via `Lagged` + re-attach with `last_seqno`. PR6 hardens
                // the backpressure path.
                let _ = tx.try_send(msg.clone());
            }
        }
        debug!(%peer_id, "federation feed loop ended (upstream closed)");
    })
}

/// Rewrite a remote [`SessionEntry`] into its local form: a freshly-assigned
/// word, local pane IDs, a peer-decorated display name, and cleared
/// `attached_clients` (the remote's client IDs are meaningless locally).
fn localize_entry(mut entry: SessionEntry, local_word: &str, peer_id: &str) -> SessionEntry {
    entry.meta.name = format!("{} @ {peer_id}", entry.meta.name);
    entry.meta.word_id = local_word.to_string();
    for pane in &mut entry.panes {
        pane.pane_id = format!("{local_word}/{}", pane.pane_index);
        pane.attached_clients.clear();
    }
    entry
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

    #[test]
    fn viewer_registration_tracks_last_viewer() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut conn = PeerConnection::new(tx);
        let (d1, _r1) = mpsc::channel(8);
        let (d2, _r2) = mpsc::channel(8);
        conn.add_viewer("hawk/0", ClientId(1), d1);
        conn.add_viewer("hawk/0", ClientId(2), d2);
        assert_eq!(conn.viewers_of("hawk/0").len(), 2);
        // Removing one viewer is not the last; removing the second is.
        assert!(!conn.remove_viewer("hawk/0", ClientId(1)));
        assert!(conn.remove_viewer("hawk/0", ClientId(2)));
        assert!(conn.viewers_of("hawk/0").is_empty());
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
}
