pub(super) mod ansi_emit;
mod attach;
mod crud;
mod helpers;
mod io;
mod pane_crud;
mod persistence;
pub(super) mod restore;

pub use attach::{AttachParams, AttachResult, InputLockOutcome};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kmux_protocol::TransportKind;
use kmux_protocol::control_rpc::{ConnectionInfo, SessionConnections, SessionsResponse};
use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ConnectionId, InputMode, SessionStatus, TermSize, epoch_millis,
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
    /// Rendering capabilities declared by this client at Auth time.
    pub capabilities: ClientCapabilities,
    /// Terminal size reported by this client at attach time (updated on `Resize`).
    pub size: TermSize,
}

/// Shared map of per-client output senders for a single pane.
pub type ClientMap = Arc<Mutex<HashMap<ClientId, ClientSender>>>;

/// Backend event sink that owns the canonical title string for a pane and
/// broadcasts `PaneTitleChanged` events to every currently-attached client
/// whenever the VT emulator reports a new OSC 0/2 title.
///
/// The `on_title` callback is invoked from the VT parser loop, so sends use
/// non-blocking channel operations only.
pub struct PaneTitleSink {
    pane_id: String,
    title: Arc<Mutex<String>>,
    clients: ClientMap,
}

impl PaneTitleSink {
    pub fn new(pane_id: String, title: Arc<Mutex<String>>, clients: ClientMap) -> Self {
        Self {
            pane_id,
            title,
            clients,
        }
    }
}

impl crate::backend::BackendEventSink for PaneTitleSink {
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
        let map = self.clients.lock().unwrap();
        for sender in map.values() {
            let _ = sender.ctrl_tx.send(event.clone());
        }
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
            let _ = sender.ctrl_tx.send(event_msg.clone());
            let _ = sender.data_tx.try_send(snap_msg.clone());
        }
    }
}

/// State for one session: its metadata plus all its panes.
pub struct SessionState {
    pub meta: kmux_protocol::messages::SessionMeta,
    /// Map of pane_index -> PaneRelay.
    pub panes: HashMap<u32, PaneRelay>,
    /// Next pane index to assign (monotonically increasing within this session).
    pub next_pane_index: u32,
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
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        let (conn_count_tx, _) = watch::channel(0usize);
        Self {
            manager: Arc::new(PtyRegistry::new()),
            auth_token: token,
            sessions: RwLock::new(HashMap::new()),
            session_index_counter: AtomicU32::new(0),
            next_client_id: AtomicU64::new(1),
            next_connection_id: AtomicU64::new(1),
            connections: RwLock::new(HashMap::new()),
            wordlist: Mutex::new(WordlistSampler::new()),
            rng: Mutex::new(rand::rngs::SmallRng::from_rng(&mut rand::rng())),
            conn_count_tx,
        }
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
    pub async fn register_client(
        &self,
        transport: TransportKind,
        new_metrics: Arc<ConnectionMetrics>,
        incoming_conn_id: Option<ConnectionId>,
    ) -> (ClientId, ConnectionId, Arc<ConnectionMetrics>) {
        if let Some(conn_id) = incoming_conn_id {
            // Resume an existing connection (channel switch in progress).
            let mut conns = self.connections.write().await;
            if let Some(state) = conns.get_mut(&conn_id.0) {
                state.transport = transport;
                let metrics = Arc::clone(&state.metrics);
                return (state.client_id, conn_id, metrics);
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
        (client_id, conn_id, new_metrics)
    }

    /// Mark a channel switch as complete. Called when the new channel sends
    /// `ChannelReady`. Returns the old transport name for the `ChannelSwitched`
    /// response, or `None` if the connection ID is unknown.
    pub async fn complete_channel_switch(
        &self,
        conn_id: ConnectionId,
        _client_id: ClientId,
    ) -> Option<String> {
        let mut conns = self.connections.write().await;
        if let Some(state) = conns.get_mut(&conn_id.0) {
            let old_name = state.transport.to_string();
            state.transport = TransportKind::Quic;
            return Some(old_name);
        }
        None
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
        let (_, c1, _) = app
            .register_client(TransportKind::Uds, Arc::clone(&m1), None)
            .await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 1);

        let m2 = Arc::new(ConnectionMetrics::new());
        let (_, c2, _) = app.register_client(TransportKind::Tcp, m2, None).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 2);

        app.unregister_client(c1).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 1);

        app.unregister_client(c2).await;
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), 0);
    }

    #[tokio::test]
    async fn register_client_assigns_fresh_ids_and_stores_metrics() {
        let app = ServerApp::new("tok".to_string());
        let metrics = Arc::new(ConnectionMetrics::new());
        metrics.bytes_in.store(42, Ordering::Relaxed);

        let (client_id, conn_id, returned_metrics) = app
            .register_client(TransportKind::Uds, Arc::clone(&metrics), None)
            .await;

        // IDs start at 1.
        assert_eq!(client_id.0, 1);
        assert_eq!(conn_id.0, 1);
        // The same Arc is returned, so counter mutations are visible.
        returned_metrics.bytes_in.fetch_add(8, Ordering::Relaxed);
        assert_eq!(metrics.bytes_in.load(Ordering::Relaxed), 50);
    }

    #[tokio::test]
    async fn channel_switch_reuses_existing_metrics() {
        let app = ServerApp::new("tok".to_string());
        let original_metrics = Arc::new(ConnectionMetrics::new());
        original_metrics.bytes_in.store(100, Ordering::Relaxed);

        let (_, conn_id, _) = app
            .register_client(TransportKind::Tcp, Arc::clone(&original_metrics), None)
            .await;

        // Simulate a channel switch: re-register with a new metrics Arc.
        let new_metrics = Arc::new(ConnectionMetrics::new());
        let (_, same_conn_id, reused_metrics) = app
            .register_client(TransportKind::Quic, Arc::clone(&new_metrics), Some(conn_id))
            .await;

        assert_eq!(same_conn_id, conn_id);
        // Should return the *original* metrics, not the new one.
        assert_eq!(reused_metrics.bytes_in.load(Ordering::Relaxed), 100);

        // complete_channel_switch should report the old transport and flip to Quic.
        let client_id = kmux_protocol::messages::ClientId(1); // placeholder
        let old = app.complete_channel_switch(conn_id, client_id).await;
        // Transport is now Quic after channel switch.
        assert_eq!(old, Some("QUIC".to_string()));
    }

    #[tokio::test]
    async fn unregister_removes_connection() {
        let app = ServerApp::new("tok".to_string());
        let metrics = Arc::new(ConnectionMetrics::new());
        let (_, conn_id, _) = app.register_client(TransportKind::Uds, metrics, None).await;
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
}
