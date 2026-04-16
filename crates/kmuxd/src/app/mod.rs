pub(super) mod ansi_emit;
mod attach;
mod crud;
mod helpers;
mod io;
mod pane_crud;
mod persistence;
pub(super) mod restore;

pub use attach::{AttachResult, InputLockOutcome};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ConnectionId, InputMode, SessionStatus, TermSize,
};
use kmux_pty::events::SessionEvent;
use kmux_pty::registry::SessionManager as PtyRegistry;
use kmux_pty::session::PtyWriter;
use rand::SeedableRng;
use tokio::sync::{RwLock, broadcast, mpsc};

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
}

/// Shared map of per-client output senders for a single pane.
pub type ClientMap = Arc<Mutex<HashMap<ClientId, ClientSender>>>;

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
    /// Shared with `WezTermBackend`; updated on every client attach/detach.
    pub kitty_graphics_enabled: Arc<AtomicBool>,
    /// Live toggle for kitty keyboard protocol in the backend emulator.
    /// Shared with `WezTermBackend`; updated on every client attach/detach.
    pub kitty_keyboard_enabled: Arc<AtomicBool>,
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
}

/// State for one session: its metadata plus all its panes.
pub struct SessionState {
    pub meta: kmux_protocol::messages::SessionMeta,
    /// Map of pane_index -> PaneRelay.
    pub panes: HashMap<u32, PaneRelay>,
    /// Next pane index to assign (monotonically increasing within this session).
    pub next_pane_index: u32,
}

/// Active transport kind for a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Tcp,
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportKind::Quic => write!(f, "quic"),
            TransportKind::Tcp => write!(f, "tcp"),
        }
    }
}

/// Per-connection state tracked by `ServerApp` for channel switching.
struct ConnectionState {
    client_id: ClientId,
    transport: TransportKind,
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
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        Self {
            manager: Arc::new(PtyRegistry::new()),
            auth_token: token,
            sessions: RwLock::new(HashMap::new()),
            session_index_counter: AtomicU32::new(0),
            next_client_id: AtomicU64::new(1),
            next_connection_id: AtomicU64::new(1),
            connections: RwLock::new(HashMap::new()),
            wordlist: Mutex::new(WordlistSampler::new()),
            rng: Mutex::new(rand::rngs::SmallRng::from_os_rng()),
        }
    }

    /// Register a client connection, returning a `(ClientId, ConnectionId)` pair.
    ///
    /// If `incoming_conn_id` is `Some`, the client is resuming an existing
    /// connection (channel switch). The old transport entry is updated in-place.
    /// If `None`, a fresh `ClientId` and `ConnectionId` are assigned.
    pub async fn register_client(
        &self,
        incoming_conn_id: Option<ConnectionId>,
    ) -> (ClientId, ConnectionId) {
        if let Some(conn_id) = incoming_conn_id {
            // Resume an existing connection (channel switch in progress).
            let mut conns = self.connections.write().await;
            if let Some(state) = conns.get_mut(&conn_id.0) {
                // Mark this as the new active transport (TCP default; QUIC upgrade handled via
                // complete_channel_switch).
                state.transport = TransportKind::Tcp;
                return (state.client_id, conn_id);
            }
            // Unknown ConnectionId — treat as a fresh connection.
        }
        let client_id = ClientId(self.next_client_id.fetch_add(1, Ordering::Relaxed));
        let conn_id = ConnectionId(self.next_connection_id.fetch_add(1, Ordering::Relaxed));
        self.connections.write().await.insert(
            conn_id.0,
            ConnectionState {
                client_id,
                transport: TransportKind::Tcp,
            },
        );
        (client_id, conn_id)
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
            // The QUIC upgrade probe sends ChannelReady after a successful QUIC auth;
            // update transport to Quic.
            state.transport = TransportKind::Quic;
            return Some(old_name);
        }
        None
    }

    /// Remove the connection entry when a client disconnects.
    pub async fn unregister_client(&self, conn_id: ConnectionId) {
        self.connections.write().await.remove(&conn_id.0);
    }

    /// Subscribe to the PTY lifecycle event bus.
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }
}
