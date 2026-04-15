use serde::{Deserialize, Serialize};

pub type RequestId = u64;

/// Opaque connection identity assigned by the server on first authentication.
///
/// Survives transport switches: when a client re-authenticates on a new channel
/// (QUIC ↔ TCP) it passes its `ConnectionId` so the server can transfer all
/// pane attachments to the new transport without the client needing to re-attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub u64);

/// Unique word-based session identifier (a single word from the EFF long wordlist).
/// Example: `"eagle"`, `"falcon"`.
pub type WordId = String;

/// Pane identifier: `"{word_id}/{pane_index}"`.
/// Example: `"eagle/0"`, `"eagle/1"`.
pub type PaneId = String;

/// Rendering capabilities self-declared by a client at Auth time.
///
/// The daemon uses these to decide which PTY environment variables to set
/// for spawned shells and which features to enable in the server-side VT
/// emulator (currently `tattoy-wezterm-term`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Client can render kitty graphics protocol image data.
    pub kitty_graphics: bool,
    /// Client can encode keyboard input using the kitty keyboard protocol.
    pub kitty_keyboard: bool,
    /// Client can display 24-bit (truecolor) RGB cells directly.
    /// The daemon always sets `COLORTERM=truecolor` today, but this field
    /// is reserved for future per-client downgrade in the forwarding layer.
    pub truecolor: bool,
    /// Client's native host `$TERM` (informational; not used for `TERM` selection).
    pub term: Option<String>,
    /// Client's self-reported `$TERM_PROGRAM` (informational).
    pub term_program: Option<String>,
}

/// Opaque client identity assigned by the server on successful authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub u64);

/// Monotonic sequence number attached to each PTY output chunk per pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SequenceNo(pub u64);

/// Terminal dimensions (rows x columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

/// Whether a PTY child process is still running or has exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Immutable session-level metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Chronological creation index (0-based, monotonically increasing).
    pub index: u32,
    /// Unique word-based identifier (e.g. `"eagle"`).
    pub word_id: WordId,
    /// Human-readable display name (default: `basename(cwd)`).
    pub name: String,
    /// Server-side working directory associated with this session.
    pub cwd: String,
}

/// Snapshot of a single pane within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Full pane identifier: `"{word_id}/{pane_index}"`.
    pub pane_id: PaneId,
    /// Zero-based index within the session (monotonically increasing per session).
    pub pane_index: u32,
    /// Shell or program running inside this pane.
    pub program: String,
    pub size: TermSize,
    /// IDs of currently attached clients.
    pub attached_clients: Vec<ClientId>,
    /// Whether the pane's child process is still running.
    pub status: SessionStatus,
}

/// Full session listing entry returned by `SessionList` and related messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    pub panes: Vec<PaneInfo>,
}

/// Input control mode for a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    /// Any authenticated client may send input.
    Open,
    /// Only the identified client may send input.
    Locked(ClientId),
    /// No client may send input (read-only).
    Disabled,
}

/// Lifecycle event relayed from the server's event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEventMsg {
    /// A new session (with its initial pane) was created.
    SessionCreated { word_id: WordId },
    /// A session and all its panes were closed.
    SessionClosed { word_id: WordId },
    /// A session was renamed.
    SessionRenamed { word_id: WordId, new_name: String },

    /// A new pane was spawned inside a session.
    PaneSpawned { pane_id: PaneId },
    /// A pane's child process exited.
    PaneExited {
        pane_id: PaneId,
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// A pane was resized.
    PaneResized {
        pane_id: PaneId,
        rows: u16,
        cols: u16,
    },
    /// A pane was closed.
    PaneClosed { pane_id: PaneId },
}

/// Error codes for structured error responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    AuthFailed,
    SessionNotFound,
    SessionAlreadyExists,
    NotAuthenticated,
    InvalidMessage,
    InternalError,
    InputLocked,
    InputDisabled,
    /// The daemon has reached the 1000 active session limit.
    SessionLimitReached,
    /// The specified pane was not found.
    PaneNotFound,
}
