pub(super) mod ansi_emit;
mod attach;
mod crud;
mod helpers;
mod io;
pub(super) mod layout;
mod migrate;
mod pane_crud;
/// Always-compiled wrappers that route pane/session operations to the peer
/// federation subsystem when it is enabled. The wrappers exist unconditionally
/// (returning the no-op / "not supported" answer when the `federation` feature
/// is off) so the dispatch layer never needs `#[cfg]` directives.
mod peer_api;
mod persistence;
pub(super) mod restore;
mod tab_crud;

pub use attach::{AttachParams, AttachResult, InputLockOutcome};
pub use tab_crud::PaneCloseOutcome;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kmux_protocol::TransportKind;
use kmux_protocol::control_rpc::{ConnectionInfo, SessionConnections, SessionsResponse};
use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ConnectionId, InputMode, PaneProgressState, SessionStatus,
    TermSize, epoch_millis,
};
use kmux_pty::events::SessionEvent;
use kmux_pty::registry::SessionManager as PtyRegistry;
use kmux_pty::session::PtyWriter;
use rand::SeedableRng as _;
use tokio::sync::{RwLock, broadcast, mpsc, watch};

use crate::capability::intersect_for_atomics;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;
use crate::wordlist::WordlistSampler;

/// 10 MB scrollback buffer per pane (estimated diff size).
pub(super) const SCROLLBACK_CAPACITY: usize = 10 * 1024 * 1024;

/// Maximum number of active sessions per daemon.
pub(super) const MAX_SESSIONS: usize = 1000;

/// Per-client sender pair: bounded data channel (for diffs) + unbounded control
/// channel (for notifications like `Lagged` that must never be dropped).
pub struct ClientSender {
    pub data_tx: mpsc::Sender<kmux_protocol::messages::ServerMessage>,
    pub ctrl_tx: mpsc::UnboundedSender<kmux_protocol::messages::ServerMessage>,
    /// When true, the relay sends full `TerminalSnapshot` messages instead
    /// of incremental `TerminalUpdate` diffs.
    pub force_full_snapshot: bool,
    /// When true, the relay skips pushing terminal-output frames to this client
    /// entirely (issue #68 connection pausing). The pane keeps running and this
    /// client keeps counting toward the effective pane size; on resume the
    /// client re-attaches and the daemon reconciles to the final state.
    pub paused: bool,
    /// Rendering capabilities declared by this client at Auth time.
    pub capabilities: ClientCapabilities,
    /// Terminal size reported by this client at attach time (updated on `Resize`).
    pub size: TermSize,
}

/// Shared map of per-client output senders for a single pane.
pub type ClientMap = Arc<Mutex<HashMap<ClientId, ClientSender>>>;

/// Latest OSC 9;4 progress for a pane: the state plus the optional `0..=100`
/// percentage. Stored in the relay and updated by `PaneEventSink::on_progress`,
/// then read into `PaneInfo` snapshots so late-attaching clients see the bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaneProgress {
    pub state: PaneProgressState,
    pub progress: Option<u8>,
}

/// Backend event sink for a single pane. Owns the canonical title string and
/// broadcasts VT-originated events (`PaneTitleChanged` for OSC 0/2,
/// `PaneClipboardCopy` for OSC 52) to all connected clients via the server-wide
/// VT events channel whenever the VT emulator reports them.
///
/// The callbacks are invoked from the VT parser loop, so each send uses a
/// non-blocking operation only. Broadcasting server-wide (rather than only to
/// clients attached to this specific pane) keeps the tab bar's titles live for
/// every pane; per-client policy (e.g. only honoring OSC 52 from the focused
/// pane) is applied on the client side.
pub struct PaneEventSink {
    pane_id: String,
    title: Arc<Mutex<String>>,
    progress: Arc<Mutex<PaneProgress>>,
    vt_events: broadcast::Sender<kmux_protocol::messages::ServerMessage>,
}

impl PaneEventSink {
    pub fn new(
        pane_id: String,
        title: Arc<Mutex<String>>,
        progress: Arc<Mutex<PaneProgress>>,
        vt_events: broadcast::Sender<kmux_protocol::messages::ServerMessage>,
    ) -> Self {
        Self {
            pane_id,
            title,
            progress,
            vt_events,
        }
    }
}

impl crate::backend::BackendEventSink for PaneEventSink {
    fn on_title(&self, title: &str) {
        {
            let mut current = self.title.lock().unwrap();
            if *current == title {
                return;
            }
            *current = title.to_string();
        }
        let event = kmux_protocol::messages::ServerMessage::Event {
            event: kmux_protocol::messages::SessionEventMsg::PaneTitleChanged {
                pane_id: self.pane_id.clone(),
                title: title.to_string(),
            },
        };
        // Ignore error: no receivers means no clients are currently connected.
        let _ = self.vt_events.send(event);
    }

    fn on_osc52_copy(&self, selection: &str, base64_data: &str) {
        let event = kmux_protocol::messages::ServerMessage::Event {
            event: kmux_protocol::messages::SessionEventMsg::PaneClipboardCopy {
                pane_id: self.pane_id.clone(),
                selection: selection.to_string(),
                data: base64_data.to_string(),
            },
        };
        // Ignore error: no receivers means no clients are currently connected.
        let _ = self.vt_events.send(event);
    }

    fn on_progress(&self, state: PaneProgressState, progress: Option<u8>) {
        {
            let mut current = self.progress.lock().unwrap();
            if current.state == state && current.progress == progress {
                return;
            }
            current.state = state;
            current.progress = progress;
        }
        let event = kmux_protocol::messages::ServerMessage::Event {
            event: kmux_protocol::messages::SessionEventMsg::PaneProgressChanged {
                pane_id: self.pane_id.clone(),
                state,
                progress,
            },
        };
        // Ignore error: no receivers means no clients are currently connected.
        let _ = self.vt_events.send(event);
    }
}

/// Per-pane relay state (formerly `SessionRelay`).
pub struct PaneRelay {
    /// Per-client output senders, shared with the relay task.
    pub clients: ClientMap,
    /// Write half of the split pane, used to forward client input.
    pub writer: PtyWriter,
    /// Background task that reads from the PTY and sends to each client.
    pub _task: tokio::task::JoinHandle<()>,
    /// Program name, stored for `SessionList` responses and session restore.
    pub program: String,
    /// Arguments passed to the program at spawn time, stored for session restore.
    pub args: Vec<String>,
    /// Current terminal size.
    pub size: TermSize,
    /// Ring buffer of recent diffs, keyed by sequence number.
    pub scrollback: Arc<Mutex<DiffBuffer>>,
    /// Server-side VT emulation state for this pane.
    pub term_state: Arc<Mutex<TermState>>,
    /// Monotonic seqno counter shared with the relay diff loop.
    pub seqno_counter: Arc<AtomicU64>,
    /// Input control mode for this pane.
    pub input_mode: InputMode,
    /// Pane lifecycle status (Running or Exited).
    pub status: SessionStatus,
    /// Live toggle for kitty graphics protocol in the backend emulator.
    /// Shared with the active terminal backend via `CapabilityHandles`;
    /// updated on every client attach/detach.
    pub kitty_graphics_enabled: Arc<AtomicBool>,
    /// Live toggle for kitty keyboard protocol in the backend emulator.
    /// Shared with the active terminal backend via `CapabilityHandles`;
    /// updated on every client attach/detach.
    pub kitty_keyboard_enabled: Arc<AtomicBool>,
    /// Latest window title reported by the pane via OSC 0/2. Shared with the
    /// backend event sink, which updates it and broadcasts to clients.
    pub title: Arc<Mutex<String>>,
    /// Latest OSC 9;4 progress reported by the pane. Shared with the backend
    /// event sink, which updates it and broadcasts `PaneProgressChanged`; read
    /// into `PaneInfo` snapshots so newly-attaching clients see the current bar.
    pub progress: Arc<Mutex<PaneProgress>>,
}

impl PaneRelay {
    /// Recompute the live kitty feature flags as the AND (intersection) of all
    /// currently-attached clients' declared capabilities, then store the result
    /// into the shared atomics read by the VT emulator backend.
    ///
    /// Call this after every `clients` insert or remove.
    pub fn recompute_live_capabilities(&self) {
        let clients = self.clients.lock().unwrap();
        let (graphics, keyboard) = intersect_for_atomics(clients.values().map(|s| &s.capabilities));
        self.kitty_graphics_enabled
            .store(graphics, Ordering::Relaxed);
        self.kitty_keyboard_enabled
            .store(keyboard, Ordering::Relaxed);
    }

    /// Compute the effective pane size: smallest rows and cols across all
    /// attached clients with non-zero dimensions.  Returns `None` when no
    /// clients are attached (caller keeps the last known size).
    pub fn effective_size(&self) -> Option<TermSize> {
        let map = self.clients.lock().unwrap();
        let rows = map.values().map(|s| s.size.rows).filter(|&r| r > 0).min()?;
        let cols = map.values().map(|s| s.size.cols).filter(|&c| c > 0).min()?;
        // Pixel dims from the client that determines the winning cell dimensions.
        let (pixel_width, pixel_height) = map
            .values()
            .find(|s| s.size.rows == rows && s.size.cols == cols)
            .map(|s| (s.size.pixel_width, s.size.pixel_height))
            .unwrap_or((0, 0));
        Some(TermSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
        })
    }

    /// Resize the emulator and update `self.size` if the effective size has
    /// changed.  Returns the new `TermSize` if a resize actually happened.
    ///
    /// Does NOT touch the PTY — the caller must call `manager.resize()` after
    /// releasing any locks that would prevent async execution.
    pub fn apply_effective_size(&mut self) -> Option<TermSize> {
        let new_size = self.effective_size()?;
        if new_size.rows == self.size.rows && new_size.cols == self.size.cols {
            return None;
        }
        self.term_state
            .lock()
            .unwrap()
            .resize(crate::backend::BackendSize::from(new_size));
        self.size = new_size;
        Some(new_size)
    }

    /// Send a `PaneResized` event + a forced `TerminalSnapshot` to every
    /// attached client after a size change.
    ///
    /// `ctrl_tx` channels are unbounded; `data_tx` sends are best-effort
    /// (dropped silently if the channel is full — the next diff will repaint).
    pub fn broadcast_resize(&self, pane_id: &str, new_size: TermSize, seqno: u64) {
        use kmux_protocol::messages::{SequenceNo, SessionEventMsg, epoch_millis};
        let event_msg = kmux_protocol::messages::ServerMessage::Event {
            event: SessionEventMsg::PaneResized {
                pane_id: pane_id.to_string(),
                size: new_size,
            },
        };
        let snapshot = self.term_state.lock().unwrap().snapshot();
        let snap_msg = kmux_protocol::messages::ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot,
            seqno: SequenceNo(seqno),
            sent_at_ms: epoch_millis(),
        };

        let map = self.clients.lock().unwrap();
        for sender in map.values() {
            // Paused clients (issue #68) get no resize snapshot; the fresh
            // snapshot they pull on resume already reflects the final size.
            if sender.paused {
                continue;
            }
            let _ = sender.ctrl_tx.send(event_msg.clone());
            let _ = sender.data_tx.try_send(snap_msg.clone());
        }
    }
}

/// One tab: a named tiling layout over a subset of the session's panes.
///
/// The layout tree and `focused_pane` are **server-authoritative and shared**
/// across all clients viewing the tab. The `panes` relay map on
/// [`SessionState`] remains the single source of PTY truth; a tab's `layout`
/// is a view over its `pane_index` keys.
pub struct TabState {
    pub tab_index: u32,
    pub name: String,
    pub layout: kmux_protocol::messages::LayoutNode,
    /// `pane_index` of the focused leaf within this tab.
    pub focused_pane: u32,
}

impl TabState {
    /// Snapshot this tab as a wire [`TabInfo`].
    pub fn to_info(&self) -> kmux_protocol::messages::TabInfo {
        kmux_protocol::messages::TabInfo {
            tab_index: self.tab_index,
            name: self.name.clone(),
            layout: self.layout.clone(),
            focused_pane: self.focused_pane,
        }
    }
}

/// State for one session: its metadata, all its panes, and its tabs.
pub struct SessionState {
    pub meta: kmux_protocol::messages::SessionMeta,
    /// Map of pane_index -> PaneRelay (the flat pool of PTYs; tabs reference
    /// these by `pane_index`).
    pub panes: HashMap<u32, PaneRelay>,
    /// Next pane index to assign (monotonically increasing within this session).
    pub next_pane_index: u32,
    /// The session's tabs (tiling layouts). Always non-empty for a live session.
    pub tabs: Vec<TabState>,
    /// Next tab index to assign (monotonically increasing within this session).
    pub next_tab_index: u32,
    /// Default/restored tab view (which tab a client views is client-local).
    pub active_tab: u32,
}

impl SessionState {
    /// Find a tab by index.
    pub fn tab(&self, tab_index: u32) -> Option<&TabState> {
        self.tabs.iter().find(|t| t.tab_index == tab_index)
    }

    /// Find a tab by index (mutable).
    pub fn tab_mut(&mut self, tab_index: u32) -> Option<&mut TabState> {
        self.tabs.iter_mut().find(|t| t.tab_index == tab_index)
    }

    /// Snapshot all tabs as wire [`TabInfo`]s.
    pub fn tab_infos(&self) -> Vec<kmux_protocol::messages::TabInfo> {
        self.tabs.iter().map(TabState::to_info).collect()
    }

    /// The tab (index) that contains the given pane, if any.
    pub fn tab_of_pane(&self, pane_index: u32) -> Option<u32> {
        self.tabs
            .iter()
            .find(|t| t.layout.leaves().contains(&pane_index))
            .map(|t| t.tab_index)
    }
}

/// Per-connection counters updated on every inbound/outbound frame.
///
/// Stored behind an `Arc` so that the I/O tasks can increment atomics without
/// holding the `ServerApp::connections` write lock.  The `Arc` is cloned into
/// the writer task and reader loop at the start of `run_client_session`; it is
/// also stored in `ConnectionState` so that `snapshot_sessions_with_connections`
/// can read a consistent snapshot.
pub struct ConnectionMetrics {
    pub created_at: Instant,
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    /// What `bytes_out` would have been with no compression (same framing).
    /// `bytes_out / bytes_out_uncompressed` is the realised compression ratio.
    pub bytes_out_uncompressed: AtomicU64,
    pub msgs_in: AtomicU64,
    pub msgs_out: AtomicU64,
    /// Epoch-ms timestamp of the last inbound frame; 0 = none yet.
    pub last_activity_ms: AtomicU64,
    /// Epoch-ms timestamp of the last successful Pong from the client; 0 = none yet.
    pub last_pong_ms: AtomicU64,
    /// Most recent measured ping RTT in milliseconds; u64::MAX = unknown.
    pub last_rtt_ms: AtomicU64,
    /// `(seq, Instant)` of the most recently sent server-originated Ping, used to
    /// compute RTT when the matching Pong arrives.
    pub last_ping_sent: Mutex<Option<(u64, Instant)>>,
}

impl ConnectionMetrics {
    pub fn new() -> Self {
        Self {
            created_at: Instant::now(),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            bytes_out_uncompressed: AtomicU64::new(0),
            msgs_in: AtomicU64::new(0),
            msgs_out: AtomicU64::new(0),
            last_activity_ms: AtomicU64::new(0),
            last_pong_ms: AtomicU64::new(0),
            last_rtt_ms: AtomicU64::new(u64::MAX),
            last_ping_sent: Mutex::new(None),
        }
    }
}

/// Per-connection state tracked by `ServerApp` for channel switching.
struct ConnectionState {
    client_id: ClientId,
    transport: TransportKind,
    metrics: Arc<ConnectionMetrics>,
}

/// Shared server state -- wrapped in `Arc` and cloned into each connection task.
pub struct ServerApp {
    /// PTY registry for spawning and managing child processes.
    pub manager: Arc<PtyRegistry>,
    pub auth_token: String,
    /// Wire compression policy applied to server→client traffic.
    pub compression: crate::config::CompressionConfig,
    /// Map of word_id -> SessionState.
    pub(super) sessions: RwLock<HashMap<kmux_protocol::messages::WordId, SessionState>>,
    /// Monotonic session creation counter.
    pub(super) session_index_counter: AtomicU32,
    /// Monotonic client ID counter.
    next_client_id: AtomicU64,
    /// Monotonic connection ID counter.
    next_connection_id: AtomicU64,
    /// Map of ConnectionId -> ConnectionState for channel switching.
    connections: RwLock<HashMap<u64, ConnectionState>>,
    /// Word pool for assigning unique session IDs.
    pub(super) wordlist: Mutex<WordlistSampler>,
    /// RNG for word selection (seeded once at startup).
    pub(super) rng: Mutex<rand::rngs::SmallRng>,
    /// Broadcasts the live connection count; used for idle-shutdown tracking.
    conn_count_tx: watch::Sender<usize>,
    /// Server-wide broadcast channel for VT-derived events (e.g. title changes).
    /// Every connected client subscribes so that tab-bar titles update for all
    /// panes, not only the one the client is currently attached to.
    vt_events_tx: broadcast::Sender<kmux_protocol::messages::ServerMessage>,
    /// Federation subsystem: upstream connections to remote `kmuxd`s whose
    /// sessions are proxied to local GUIs (issue #121). See
    /// [`crate::federation`] and `docs/architecture-federation.md`.
    #[cfg(feature = "federation")]
    pub(super) peer_manager: crate::federation::PeerManager,
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        let (conn_count_tx, _) = watch::channel(0usize);
        let (vt_events_tx, _) = broadcast::channel(512);
        Self {
            manager: Arc::new(PtyRegistry::new()),
            auth_token: token,
            compression: crate::config::CompressionConfig::default(),
            sessions: RwLock::new(HashMap::new()),
            session_index_counter: AtomicU32::new(0),
            next_client_id: AtomicU64::new(1),
            next_connection_id: AtomicU64::new(1),
            connections: RwLock::new(HashMap::new()),
            wordlist: Mutex::new(WordlistSampler::new()),
            rng: Mutex::new(rand::rngs::SmallRng::from_rng(&mut rand::rng())),
            conn_count_tx,
            vt_events_tx,
            #[cfg(feature = "federation")]
            peer_manager: crate::federation::PeerManager::new(),
        }
    }

    /// Override the wire compression policy (from `kmuxd.toml`). Builder-style so
    /// `startup.rs` can configure it before wrapping the app in an `Arc`.
    pub fn with_compression(mut self, compression: crate::config::CompressionConfig) -> Self {
        self.compression = compression;
        self
    }

    /// Subscribe to VT-derived events (e.g. `PaneTitleChanged`) that are
    /// broadcast server-wide to all connected clients.
    pub fn subscribe_vt_events(
        &self,
    ) -> broadcast::Receiver<kmux_protocol::messages::ServerMessage> {
        self.vt_events_tx.subscribe()
    }

    /// Broadcast a server message to every connected client via the server-wide
    /// event channel. Used for layout updates and tab lifecycle events, which —
    /// like title changes — must reach all clients viewing a session, not just
    /// those attached to one pane. Best-effort (no receivers = no-op).
    pub fn broadcast(&self, msg: kmux_protocol::messages::ServerMessage) {
        let _ = self.vt_events_tx.send(msg);
    }

    /// Broadcast the authoritative layout tree for a tab to all clients.
    pub fn broadcast_layout(
        &self,
        word_id: &str,
        tab_index: u32,
        layout: kmux_protocol::messages::LayoutNode,
        focused_pane: u32,
    ) {
        self.broadcast(kmux_protocol::messages::ServerMessage::LayoutUpdate {
            word_id: word_id.to_string(),
            tab_index,
            layout,
            focused_pane,
        });
    }

    /// Broadcast a session lifecycle event (tab created/closed/renamed, layout
    /// changed) to all connected clients.
    pub fn broadcast_session_event(&self, event: kmux_protocol::messages::SessionEventMsg) {
        self.broadcast(kmux_protocol::messages::ServerMessage::Event { event });
    }

    /// Subscribe to live connection-count changes for idle-shutdown tracking.
    pub fn conn_count_rx(&self) -> watch::Receiver<usize> {
        self.conn_count_tx.subscribe()
    }

    /// Register a client connection, returning a `(ClientId, ConnectionId)` pair
    /// and the `Arc<ConnectionMetrics>` to use for this connection going forward.
    ///
    /// If `incoming_conn_id` is `Some`, the client is resuming an existing
    /// connection (channel switch). The transport label is updated and the
    /// *existing* `Arc<ConnectionMetrics>` is returned so byte counters accumulate
    /// across transport switches.  `new_metrics` is discarded in that case.
    /// If `None`, a fresh `ClientId` and `ConnectionId` are assigned and
    /// `new_metrics` is stored.
    /// Register a freshly-connected client or resume an existing connection
    /// after a channel switch.
    ///
    /// Returns `(client_id, conn_id, metrics, previous_transport)`. The last
    /// element is `Some(old)` when a channel switch is in progress (the
    /// caller must remember this and send it back in `ChannelSwitched`
    /// once the new channel signals `ChannelReady`); `None` when this is
    /// the first connection for the conn_id.
    pub async fn register_client(
        &self,
        transport: TransportKind,
        new_metrics: Arc<ConnectionMetrics>,
        incoming_conn_id: Option<ConnectionId>,
    ) -> (
        ClientId,
        ConnectionId,
        Arc<ConnectionMetrics>,
        Option<TransportKind>,
    ) {
        if let Some(conn_id) = incoming_conn_id {
            // Resume an existing connection (channel switch in progress).
            let mut conns = self.connections.write().await;
            if let Some(state) = conns.get_mut(&conn_id.0) {
                let previous = state.transport;
                state.transport = transport;
                let metrics = Arc::clone(&state.metrics);
                return (state.client_id, conn_id, metrics, Some(previous));
            }
            // Unknown ConnectionId — treat as a fresh connection.
        }
        let client_id = ClientId(self.next_client_id.fetch_add(1, Ordering::Relaxed));
        let conn_id = ConnectionId(self.next_connection_id.fetch_add(1, Ordering::Relaxed));
        let count = {
            let mut conns = self.connections.write().await;
            conns.insert(
                conn_id.0,
                ConnectionState {
                    client_id,
                    transport,
                    metrics: Arc::clone(&new_metrics),
                },
            );
            conns.len()
        };
        let _ = self.conn_count_tx.send(count);
        (client_id, conn_id, new_metrics, None)
    }

    /// Remove the connection entry when a client disconnects.
    pub async fn unregister_client(&self, conn_id: ConnectionId) {
        let count = {
            let mut conns = self.connections.write().await;
            conns.remove(&conn_id.0);
            conns.len()
        };
        let _ = self.conn_count_tx.send(count);
    }

    /// Snapshot all sessions and their attached connections for the `"sessions"`
    /// control-socket command.  Both locks are held simultaneously to avoid
    /// tearing between the two maps.
    pub async fn snapshot_sessions_with_connections(&self) -> SessionsResponse {
        let sessions = self.sessions.read().await;
        let conns = self.connections.read().await;

        let now_ms = epoch_millis();

        // Build ClientId → ConnectionInfo for quick lookup.
        let conn_by_client: HashMap<ClientId, ConnectionInfo> = conns
            .iter()
            .map(|(conn_id_u64, state)| {
                let m = &state.metrics;
                let last_activity_ms = m.last_activity_ms.load(Ordering::Relaxed);
                let last_pong_ms = m.last_pong_ms.load(Ordering::Relaxed);
                let last_rtt_ms = m.last_rtt_ms.load(Ordering::Relaxed);
                let info = ConnectionInfo {
                    connection_id: *conn_id_u64,
                    client_id: state.client_id.0,
                    transport: state.transport.to_string(),
                    bytes_in: m.bytes_in.load(Ordering::Relaxed),
                    bytes_out: m.bytes_out.load(Ordering::Relaxed),
                    msgs_in: m.msgs_in.load(Ordering::Relaxed),
                    msgs_out: m.msgs_out.load(Ordering::Relaxed),
                    uptime_secs: m.created_at.elapsed().as_secs(),
                    last_activity_ago_ms: if last_activity_ms > 0 {
                        Some(now_ms.saturating_sub(last_activity_ms))
                    } else {
                        None
                    },
                    last_pong_ago_ms: if last_pong_ms > 0 {
                        Some(now_ms.saturating_sub(last_pong_ms))
                    } else {
                        None
                    },
                    last_rtt_ms: if last_rtt_ms != u64::MAX {
                        Some(last_rtt_ms)
                    } else {
                        None
                    },
                };
                (state.client_id, info)
            })
            .collect();

        // Track which ClientIds appeared in at least one session pane.
        let mut seen_clients: std::collections::HashSet<ClientId> =
            std::collections::HashSet::new();

        let mut session_entries: Vec<SessionConnections> = sessions
            .values()
            .map(|state| {
                let attached: Vec<ClientId> = state
                    .panes
                    .values()
                    .flat_map(|relay| {
                        relay
                            .clients
                            .lock()
                            .unwrap()
                            .keys()
                            .copied()
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let connections: Vec<ConnectionInfo> = attached
                    .iter()
                    .filter_map(|cid| {
                        let info = conn_by_client.get(cid).cloned();
                        if info.is_some() {
                            seen_clients.insert(*cid);
                        }
                        info
                    })
                    .collect();
                SessionConnections {
                    meta: state.meta.clone(),
                    panes_count: state.panes.len(),
                    connections,
                }
            })
            .collect();
        session_entries.sort_by_key(|e| e.meta.index);

        let unattached: Vec<ConnectionInfo> = conn_by_client
            .into_iter()
            .filter_map(|(cid, info)| {
                if seen_clients.contains(&cid) {
                    None
                } else {
                    Some(info)
                }
            })
            .collect();

        SessionsResponse {
            sessions: session_entries,
            unattached,
        }
    }

    /// Subscribe to the PTY lifecycle event bus.
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use kmux_protocol::TransportKind;

    use super::{ConnectionMetrics, ServerApp};

    #[tokio::test]
    async fn conn_count_watch_tracks_register_and_unregister() {
        let app = ServerApp::new("tok".to_string());
        let mut rx = app.conn_count_rx();

        // Initially 0.
        assert_eq!(*rx.borrow(), 0);

        let m1 = Arc::new(ConnectionMetrics::new());
        let (_, c1, _, _) = app
            .register_client(TransportKind::Uds, Arc::clone(&m1), None)
            .await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 1);

        let m2 = Arc::new(ConnectionMetrics::new());
        let (_, c2, _, _) = app.register_client(TransportKind::Tcp, m2, None).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 2);

        app.unregister_client(c1).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 1);

        app.unregister_client(c2).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 0);
    }

    #[test]
    fn pane_event_sink_broadcasts_osc52_copy() {
        use std::sync::Mutex;

        use kmux_protocol::messages::{ServerMessage, SessionEventMsg};
        use tokio::sync::broadcast;

        use crate::backend::BackendEventSink;

        let (tx, mut rx) = broadcast::channel(8);
        let sink = super::PaneEventSink::new(
            "eagle/0".to_string(),
            Arc::new(Mutex::new(String::new())),
            Arc::new(Mutex::new(super::PaneProgress::default())),
            tx,
        );

        sink.on_osc52_copy("c", "aGVsbG8=");

        match rx.try_recv().expect("event broadcast") {
            ServerMessage::Event {
                event:
                    SessionEventMsg::PaneClipboardCopy {
                        pane_id,
                        selection,
                        data,
                    },
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(selection, "c");
                assert_eq!(data, "aGVsbG8=");
            }
            other => panic!("expected PaneClipboardCopy, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_client_assigns_fresh_ids_and_stores_metrics() {
        let app = ServerApp::new("tok".to_string());
        let metrics = Arc::new(ConnectionMetrics::new());
        metrics.bytes_in.store(42, Ordering::Relaxed);

        let (client_id, conn_id, returned_metrics, previous) = app
            .register_client(TransportKind::Uds, Arc::clone(&metrics), None)
            .await;

        // IDs start at 1.
        assert_eq!(client_id.0, 1);
        assert_eq!(conn_id.0, 1);
        // Fresh registration carries no previous transport.
        assert!(previous.is_none());
        // The same Arc is returned, so counter mutations are visible.
        returned_metrics.bytes_in.fetch_add(8, Ordering::Relaxed);
        assert_eq!(metrics.bytes_in.load(Ordering::Relaxed), 50);
    }

    #[tokio::test]
    async fn channel_switch_reuses_existing_metrics_and_reports_previous_transport() {
        let app = ServerApp::new("tok".to_string());
        let original_metrics = Arc::new(ConnectionMetrics::new());
        original_metrics.bytes_in.store(100, Ordering::Relaxed);

        let (_, conn_id, _, first_previous) = app
            .register_client(TransportKind::Tcp, Arc::clone(&original_metrics), None)
            .await;
        // Fresh registration has no previous transport.
        assert!(first_previous.is_none());

        // Simulate a channel switch: re-register with a new metrics Arc.
        let new_metrics = Arc::new(ConnectionMetrics::new());
        let (_, same_conn_id, reused_metrics, swap_previous) = app
            .register_client(TransportKind::Quic, Arc::clone(&new_metrics), Some(conn_id))
            .await;

        assert_eq!(same_conn_id, conn_id);
        // Should return the *original* metrics, not the new one.
        assert_eq!(reused_metrics.bytes_in.load(Ordering::Relaxed), 100);
        // The genuinely-old transport must come back so the caller can
        // emit `ChannelSwitched { old_transport: Tcp }` later. Previously
        // this was silently overwritten with `Quic` regardless of the
        // actual prior transport.
        assert_eq!(swap_previous, Some(TransportKind::Tcp));

        // The recorded transport for the connection now reflects the new
        // channel.
        let snap = app.snapshot_sessions_with_connections().await;
        assert_eq!(snap.unattached.len(), 1);
        assert_eq!(snap.unattached[0].transport, "QUIC");
    }

    #[tokio::test]
    async fn unregister_removes_connection() {
        let app = ServerApp::new("tok".to_string());
        let metrics = Arc::new(ConnectionMetrics::new());
        let (_, conn_id, _, _) = app.register_client(TransportKind::Uds, metrics, None).await;
        app.unregister_client(conn_id).await;

        // After unregister, snapshot_sessions_with_connections returns no unattached.
        let snap = app.snapshot_sessions_with_connections().await;
        assert!(snap.unattached.is_empty());
    }

    #[tokio::test]
    async fn snapshot_reports_correct_transport_label() {
        let app = ServerApp::new("tok".to_string());
        let metrics = Arc::new(ConnectionMetrics::new());
        let _ = app.register_client(TransportKind::Uds, metrics, None).await;

        let snap = app.snapshot_sessions_with_connections().await;
        assert_eq!(snap.unattached.len(), 1);
        assert_eq!(snap.unattached[0].transport, "UDS");
    }

    // ─── Size negotiation unit tests ──────────────────────────────────────────

    use kmux_protocol::messages::{ClientCapabilities, ClientId, TermSize};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use tokio::sync::mpsc;

    use super::{AttachParams, AttachResult, SessionState};
    use crate::app::{ClientSender, PaneRelay, SCROLLBACK_CAPACITY};
    use crate::backend::{
        BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK, NullEventSink,
    };
    use crate::scrollback::DiffBuffer;
    use crate::term_state::new_term_state;

    fn make_client(rows: u16, cols: u16) -> (ClientId, ClientSender) {
        let (data_tx, _data_rx) = mpsc::channel::<kmux_protocol::messages::ServerMessage>(16);
        let (ctrl_tx, _ctrl_rx) =
            mpsc::unbounded_channel::<kmux_protocol::messages::ServerMessage>();
        let id = ClientId(rows as u64 * 1000 + cols as u64);
        let sender = ClientSender {
            data_tx,
            ctrl_tx,
            force_full_snapshot: false,
            paused: false,
            capabilities: ClientCapabilities::default(),
            size: TermSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        };
        (id, sender)
    }

    fn make_relay(rows: u16, cols: u16) -> PaneRelay {
        use kmux_pty::session::PtyWriter;
        use std::sync::atomic::AtomicBool;
        let cfg = BackendConfig {
            size: BackendSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            capabilities: CapabilityHandles {
                kitty_graphics: Arc::new(AtomicBool::new(false)),
                kitty_keyboard: Arc::new(AtomicBool::new(false)),
            },
            events: Arc::new(NullEventSink),
            scrollback: DEFAULT_SCROLLBACK,
        };
        let term_state = Arc::new(Mutex::new(new_term_state(cfg)));
        let kitty_graphics_enabled = Arc::new(AtomicBool::new(false));
        let kitty_keyboard_enabled = Arc::new(AtomicBool::new(false));
        PaneRelay {
            clients: Arc::new(Mutex::new(std::collections::HashMap::new())),
            writer: PtyWriter::sink().unwrap(),
            _task: tokio::task::spawn(async {}),
            program: "/bin/sh".to_string(),
            args: vec![],
            size: TermSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            scrollback: Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY))),
            term_state,
            seqno_counter: Arc::new(AtomicU64::new(1)),
            input_mode: kmux_protocol::messages::InputMode::Open,
            status: kmux_protocol::messages::SessionStatus::Running,
            kitty_graphics_enabled,
            kitty_keyboard_enabled,
            title: Arc::new(Mutex::new(String::new())),
            progress: Arc::new(Mutex::new(super::PaneProgress::default())),
        }
    }

    #[tokio::test]
    async fn effective_size_min_wins() {
        let relay = make_relay(24, 80);
        let (id_a, sender_a) = make_client(24, 80);
        let (id_b, sender_b) = make_client(40, 120);
        relay.clients.lock().unwrap().insert(id_a, sender_a);
        relay.clients.lock().unwrap().insert(id_b, sender_b);

        let eff = relay.effective_size().unwrap();
        assert_eq!(eff.rows, 24, "min rows should win");
        assert_eq!(eff.cols, 80, "min cols should win");
    }

    #[tokio::test]
    async fn effective_size_no_clients_returns_none() {
        let relay = make_relay(24, 80);
        assert!(relay.effective_size().is_none());
    }

    #[tokio::test]
    async fn apply_effective_size_returns_none_when_unchanged() {
        let mut relay = make_relay(24, 80);
        let (id, sender) = make_client(24, 80);
        relay.clients.lock().unwrap().insert(id, sender);
        // effective == current → no resize
        assert!(relay.apply_effective_size().is_none());
    }

    #[tokio::test]
    async fn apply_effective_size_resizes_emulator_when_changed() {
        let mut relay = make_relay(24, 80);
        let (id, sender) = make_client(40, 120);
        relay.clients.lock().unwrap().insert(id, sender);
        // effective (40×120) differs from relay.size (24×80)
        let new_size = relay.apply_effective_size().expect("should resize");
        assert_eq!(new_size.rows, 40);
        assert_eq!(new_size.cols, 120);
        assert_eq!(relay.size.rows, 40);
        assert_eq!(relay.size.cols, 120);
    }

    #[tokio::test]
    async fn detach_keeps_last_effective_size() {
        let mut relay = make_relay(80, 200);
        let (id_a, sender_a) = make_client(24, 80);
        let (id_b, sender_b) = make_client(40, 120);
        relay.clients.lock().unwrap().insert(id_a, sender_a);
        relay.clients.lock().unwrap().insert(id_b, sender_b);
        // Effective = 24×80; apply it.
        relay.apply_effective_size();
        assert_eq!(relay.size.rows, 24);

        // Now remove all clients.
        relay.clients.lock().unwrap().clear();
        // effective_size returns None (no clients), so apply_effective_size is a no-op.
        assert!(relay.apply_effective_size().is_none());
        // Size stays at 24×80, not back to 80×200.
        assert_eq!(relay.size.rows, 24);
        assert_eq!(relay.size.cols, 80);
    }

    #[tokio::test]
    async fn broadcast_resize_emits_snapshot_before_diff() {
        use kmux_protocol::messages::{
            CursorState, SequenceNo, ServerMessage, SessionEventMsg, TermModes, TerminalDiff,
        };

        let mut relay = make_relay(24, 80);
        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(16);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let id = ClientId(1);
        relay.clients.lock().unwrap().insert(
            id,
            ClientSender {
                data_tx: data_tx.clone(),
                ctrl_tx,
                force_full_snapshot: false,
                paused: false,
                capabilities: ClientCapabilities::default(),
                size: TermSize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        );

        let new_size = relay.apply_effective_size().expect("size must change");
        relay.broadcast_resize("pane-1", new_size, 42);

        // Simulate the next diff arriving on the same data_tx (as the relay's
        // flush_cell_diff would enqueue it). FIFO guarantees it lands after
        // the snapshot.
        let followup = ServerMessage::TerminalUpdate {
            pane_id: "pane-1".to_string(),
            diff: Arc::new(TerminalDiff {
                ops: vec![],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                history_total: 0,
                scrollback_reset: None,
            }),
            seqno: SequenceNo(43),
            sent_at_ms: 0,
        };
        data_tx.try_send(followup).expect("send follow-up diff");

        // ctrl_rx gets the PaneResized event.
        let ev = ctrl_rx.try_recv().expect("PaneResized on ctrl_tx");
        assert!(
            matches!(
                ev,
                ServerMessage::Event {
                    event: SessionEventMsg::PaneResized { .. },
                }
            ),
            "expected PaneResized event, got {ev:?}"
        );

        // data_rx: snapshot first, then the diff.
        let first = data_rx.try_recv().expect("snapshot on data_tx");
        match first {
            ServerMessage::TerminalSnapshot {
                ref pane_id,
                ref snapshot,
                ..
            } => {
                assert_eq!(pane_id, "pane-1");
                assert_eq!(snapshot.rows, new_size.rows);
                assert_eq!(snapshot.cols, new_size.cols);
            }
            other => panic!("expected TerminalSnapshot, got {other:?}"),
        }

        let second = data_rx.try_recv().expect("follow-up diff on data_tx");
        assert!(
            matches!(second, ServerMessage::TerminalUpdate { .. }),
            "follow-up diff must arrive after the snapshot, got {second:?}"
        );
    }

    #[tokio::test]
    async fn paused_client_still_counts_toward_effective_size() {
        // Pausing must never reflow the PTY for other clients: a paused client
        // keeps constraining the smallest-wins effective size (issue #68).
        let relay = make_relay(80, 200);
        let (id_paused, mut sender_paused) = make_client(24, 80);
        sender_paused.paused = true;
        let (id_active, sender_active) = make_client(40, 120);
        relay
            .clients
            .lock()
            .unwrap()
            .insert(id_paused, sender_paused);
        relay
            .clients
            .lock()
            .unwrap()
            .insert(id_active, sender_active);

        let eff = relay.effective_size().unwrap();
        assert_eq!(eff.rows, 24, "paused client's smaller rows still win");
        assert_eq!(eff.cols, 80, "paused client's smaller cols still win");
    }

    #[tokio::test]
    async fn broadcast_resize_skips_paused_client() {
        use kmux_protocol::messages::ServerMessage;

        let mut relay = make_relay(24, 80);
        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(16);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        relay.clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                paused: true,
                // Larger than relay.size so apply_effective_size would change.
                capabilities: ClientCapabilities::default(),
                size: TermSize {
                    rows: 40,
                    cols: 120,
                    pixel_width: 0,
                    pixel_height: 0,
                },
            },
        );

        let new_size = relay.apply_effective_size().expect("size must change");
        relay.broadcast_resize("pane-1", new_size, 42);

        assert!(
            ctrl_rx.try_recv().is_err(),
            "paused client should get no PaneResized event"
        );
        assert!(
            data_rx.try_recv().is_err(),
            "paused client should get no resize snapshot"
        );
    }

    // ─── Resume reconciliation (issue #68) ────────────────────────────────────

    fn push_seqnos(relay: &PaneRelay, range: std::ops::RangeInclusive<u64>) {
        use kmux_protocol::messages::{
            CellState, CursorState, DiffOp, SequenceNo, TermModes, TerminalDiff,
        };
        let mut buf = relay.scrollback.lock().unwrap();
        for n in range {
            buf.push(
                SequenceNo(n),
                Arc::new(TerminalDiff {
                    ops: vec![DiffOp::Cell {
                        row: 0,
                        col: 0,
                        cell: CellState::default(),
                    }],
                    cursor: CursorState::default(),
                    modes: TermModes::EMPTY,
                    history_total: 0,
                    scrollback_reset: None,
                }),
            );
        }
    }

    #[tokio::test]
    async fn compute_replay_fresh_attach_returns_full_snapshot() {
        use super::attach::compute_replay;
        let relay = make_relay(24, 80);
        assert!(matches!(
            compute_replay(&relay, None),
            AttachResult::FullSnapshot(..)
        ));
    }

    #[tokio::test]
    async fn compute_replay_delta_under_threshold_returns_delta() {
        use super::attach::compute_replay;
        use kmux_protocol::messages::SequenceNo;
        let relay = make_relay(24, 80);
        push_seqnos(&relay, 1..=5);
        match compute_replay(&relay, Some(SequenceNo(1))) {
            AttachResult::Delta(diffs) => {
                let seqs: Vec<u64> = diffs.iter().map(|(s, _)| s.0).collect();
                assert_eq!(
                    seqs,
                    vec![2, 3, 4, 5],
                    "replays only diffs after last_seqno"
                );
            }
            other => panic!("expected Delta, got a different variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn compute_replay_delta_over_threshold_coalesces_to_syncreset() {
        use super::attach::{MAX_RESUME_DELTA_DIFFS, compute_replay};
        use kmux_protocol::messages::SequenceNo;
        let relay = make_relay(24, 80);
        // More buffered diffs than the coalescing threshold allows to replay.
        push_seqnos(&relay, 1..=(MAX_RESUME_DELTA_DIFFS as u64 + 50));
        assert!(matches!(
            compute_replay(&relay, Some(SequenceNo(1))),
            AttachResult::SyncReset(..)
        ));
    }

    async fn app_with_one_pane(word: &str) -> ServerApp {
        use kmux_protocol::messages::SessionMeta;
        let app = ServerApp::new("tok".to_string());
        let relay = make_relay(24, 80);
        let session = SessionState {
            meta: SessionMeta {
                index: 0,
                word_id: word.to_string(),
                name: "test".to_string(),
                cwd: "/".to_string(),
            },
            panes: std::iter::once((0u32, relay)).collect(),
            next_pane_index: 1,
            tabs: vec![],
            next_tab_index: 0,
            active_tab: 0,
        };
        app.sessions.write().await.insert(word.to_string(), session);
        app
    }

    fn attach_params(
        client_id: ClientId,
        last_seqno: Option<kmux_protocol::messages::SequenceNo>,
    ) -> AttachParams {
        use kmux_protocol::messages::ServerMessage;
        let (data_tx, _rx) = mpsc::channel::<ServerMessage>(16);
        let (ctrl_tx, _crx) = mpsc::unbounded_channel::<ServerMessage>();
        AttachParams {
            pane_id: "eagle/0".to_string(),
            client_id,
            last_seqno,
            size: TermSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            data_tx,
            ctrl_tx,
            capabilities: ClientCapabilities::default(),
        }
    }

    #[tokio::test]
    async fn set_paused_marks_client_across_panes() {
        let app = app_with_one_pane("eagle").await;
        let client_id = ClientId(1);
        app.attach(attach_params(client_id, None)).await.unwrap();

        app.set_paused(client_id, true).await;
        {
            let sessions = app.sessions.read().await;
            let map = sessions["eagle"].panes[&0].clients.lock().unwrap();
            assert!(map.get(&client_id).unwrap().paused);
        }

        app.set_paused(client_id, false).await;
        {
            let sessions = app.sessions.read().await;
            let map = sessions["eagle"].panes[&0].clients.lock().unwrap();
            assert!(!map.get(&client_id).unwrap().paused);
        }
    }

    #[tokio::test]
    async fn reattach_preserves_snapshot_mode_and_clears_pause() {
        use kmux_protocol::messages::SequenceNo;
        let app = app_with_one_pane("eagle").await;
        let client_id = ClientId(1);

        app.attach(attach_params(client_id, None)).await.unwrap();
        app.set_snapshot_mode(client_id, true).await;
        app.set_paused(client_id, true).await;

        // Resume: client re-attaches the pane with its last seqno.
        app.attach(attach_params(client_id, Some(SequenceNo(1))))
            .await
            .unwrap();

        let sessions = app.sessions.read().await;
        let map = sessions["eagle"].panes[&0].clients.lock().unwrap();
        let sender = map.get(&client_id).unwrap();
        assert!(
            sender.force_full_snapshot,
            "snapshot mode must survive a re-attach"
        );
        assert!(!sender.paused, "a re-attach (resume) clears the pause flag");
    }
}
