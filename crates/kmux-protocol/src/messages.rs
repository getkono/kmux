use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Current wire protocol version. Increment when breaking changes are made.
pub const PROTOCOL_VERSION: u32 = 11;

/// Return the current wall-clock time as milliseconds since the Unix epoch.
pub fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub type RequestId = u64;

/// Unique word-based session identifier (a single word from the EFF long wordlist).
/// Example: `"eagle"`, `"falcon"`.
pub type WordId = String;

/// Pane identifier: `"{word_id}/{pane_index}"`.
/// Example: `"eagle/0"`, `"eagle/1"`.
pub type PaneId = String;

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

//  VT diff types

/// Portable cell color -- resolved to RGB on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl CellColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Packed attribute bitfield.
///
/// Bit layout: bold=0, italic=1, underline=2, strikethrough=3,
/// inverse=4, hidden=5, dim=6, blink=7, wide_char=8, wide_char_spacer=9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    pub const EMPTY: Self = Self(0);
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const STRIKETHROUGH: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const DIM: u16 = 1 << 6;
    pub const BLINK: u16 = 1 << 7;
    pub const WIDE_CHAR: u16 = 1 << 8;
    pub const WIDE_CHAR_SPACER: u16 = 1 << 9;

    pub fn contains(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

/// State of a single terminal cell -- character + colors + attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellState {
    pub c: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub attrs: CellAttrs,
}

impl Default for CellState {
    fn default() -> Self {
        Self {
            c: ' ',
            fg: CellColor::new(0xab, 0xb2, 0xbf), // One Dark foreground
            bg: CellColor::new(0x28, 0x2c, 0x34), // One Dark background
            attrs: CellAttrs::EMPTY,
        }
    }
}

/// Cursor shape in the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
    HollowBlock,
    Hidden,
}

/// Cursor position and appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            row: 0,
            col: 0,
            shape: CursorShape::Block,
            visible: true,
        }
    }
}

/// Terminal mode flags sent alongside diffs.
///
/// Bit 0: APP_CURSOR (application cursor keys mode).
/// Bit 1: BRACKETED_PASTE (DEC private mode 2004).
/// Bit 2: MOUSE_REPORT_CLICK (DEC mode 1000 — normal mouse tracking).
/// Bit 3: MOUSE_DRAG (DEC mode 1002 — button-event tracking).
/// Bit 4: MOUSE_MOTION (DEC mode 1003 — any-event tracking).
/// Bit 5: SGR_MOUSE (DEC mode 1006 — SGR extended coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermModes(pub u16);

impl TermModes {
    pub const EMPTY: Self = Self(0);
    pub const APP_CURSOR: u16 = 1 << 0;
    pub const BRACKETED_PASTE: u16 = 1 << 1;
    pub const MOUSE_REPORT_CLICK: u16 = 1 << 2;
    pub const MOUSE_DRAG: u16 = 1 << 3;
    pub const MOUSE_MOTION: u16 = 1 << 4;
    pub const SGR_MOUSE: u16 = 1 << 5;

    pub fn app_cursor(self) -> bool {
        self.0 & Self::APP_CURSOR != 0
    }

    pub fn bracketed_paste(self) -> bool {
        self.0 & Self::BRACKETED_PASTE != 0
    }

    /// Whether any mouse reporting mode is active (1000, 1002, or 1003).
    pub fn mouse_report(self) -> bool {
        self.0 & (Self::MOUSE_REPORT_CLICK | Self::MOUSE_DRAG | Self::MOUSE_MOTION) != 0
    }

    /// Whether SGR extended mouse coordinates are active (mode 1006).
    pub fn sgr_mouse(self) -> bool {
        self.0 & Self::SGR_MOUSE != 0
    }
}

/// A single diff operation describing changed cells.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffOp {
    /// A single cell changed.
    Cell { row: u16, col: u16, cell: CellState },
    /// A contiguous run of cells changed on the same row.
    Row {
        row: u16,
        start_col: u16,
        cells: Vec<CellState>,
    },
    /// The entire screen was cleared.
    Clear,
}

/// A set of cell changes + cursor/mode state for one frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalDiff {
    pub ops: Vec<DiffOp>,
    pub cursor: CursorState,
    pub modes: TermModes,
    /// Lines that scrolled off the top of the visible area during this frame.
    /// Oldest first. Empty when no lines were pushed to scrollback.
    #[serde(default)]
    pub scrollback_lines: Vec<Vec<CellState>>,
}

/// Full grid snapshot -- sent on attach or after resize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// Row-major cell data (length = rows * cols).
    pub cells: Vec<CellState>,
    pub cursor: CursorState,
    pub modes: TermModes,
}

/// Messages sent from client -> server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message: authenticate with a shared token.
    Auth {
        token: String,
        /// Must equal `PROTOCOL_VERSION`; server rejects mismatches.
        protocol_version: u32,
    },

    /// Request creation of a new session (with one initial pane).
    /// The server assigns the `word_id` automatically.
    SessionCreate {
        request_id: RequestId,
        /// Optional display name; defaults to `basename(cwd)` if `None`.
        name: Option<String>,
        /// Working directory for the session (server-side path).
        /// Defaults to the server's home directory if `None`.
        cwd: Option<String>,
        /// Shell or program to run in the initial pane; defaults to system shell if `None`.
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    },

    /// Request graceful close of an entire session (all panes).
    SessionClose {
        request_id: RequestId,
        word_id: WordId,
    },

    /// Request a list of all active sessions.
    SessionList { request_id: RequestId },

    /// Rename an existing session's display name.
    SessionRename {
        request_id: RequestId,
        word_id: WordId,
        new_name: String,
    },

    /// Create a new pane inside an existing session.
    PaneCreate {
        request_id: RequestId,
        word_id: WordId,
        /// Shell or program to run; defaults to system shell if `None`.
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    },

    /// Request graceful close of a single pane.
    PaneClose {
        request_id: RequestId,
        pane_id: PaneId,
    },

    /// Send bytes to the PTY master (user keystrokes).
    PtyInput { pane_id: PaneId, data: Vec<u8> },

    /// Paste clipboard text into the PTY. The server handles bracketed-paste
    /// wrapping when the terminal has enabled DEC private mode 2004.
    PtyPaste { pane_id: PaneId, data: String },

    /// Resize the PTY window.
    Resize { pane_id: PaneId, size: TermSize },

    /// Subscribe to PTY output for a pane.
    ///
    /// `last_seqno = None`       -> send full snapshot (first attach or full resync)
    /// `last_seqno = Some(n)`    -> replay only chunks with seqno > n (reconnect)
    Attach {
        pane_id: PaneId,
        last_seqno: Option<SequenceNo>,
    },

    /// Unsubscribe from PTY output for a pane.
    Detach { pane_id: PaneId },

    /// Send a Unix signal to the PTY child process.
    Signal { pane_id: PaneId, signal: i32 },

    /// Request exclusive input rights for a pane.
    RequestInputLock { pane_id: PaneId },

    /// Release previously acquired input lock.
    ReleaseInputLock { pane_id: PaneId },

    /// Toggle full-snapshot mode for this client. When enabled, the server
    /// sends `TerminalSnapshot` messages instead of incremental `TerminalUpdate`
    /// diffs on every PTY output, bypassing the diff engine entirely.
    SetSnapshotMode { enabled: bool },

    /// Keep-alive ping (client -> server).
    Ping { seq: u64 },

    /// Response to server Ping.
    Pong { seq: u64 },
}

/// Messages sent from server -> client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Response to `Auth`.
    AuthResult {
        success: bool,
        reason: Option<String>,
        /// Assigned on success; `None` on failure.
        client_id: Option<ClientId>,
        /// Server binary version (e.g. `"0.1.0"`); `None` on failure.
        server_version: Option<String>,
    },

    /// Confirmation that a session (with initial pane) was created.
    SessionCreated {
        request_id: RequestId,
        entry: SessionEntry,
    },

    /// Confirmation that a session was closed.
    SessionClosed {
        request_id: RequestId,
        word_id: WordId,
        exit_code: Option<i32>,
    },

    /// Response to `SessionList`.
    SessionListResult {
        request_id: RequestId,
        sessions: Vec<SessionEntry>,
    },

    /// Confirmation that a new pane was created.
    PaneCreated {
        request_id: RequestId,
        pane_id: PaneId,
        session_word_id: WordId,
    },

    /// Confirmation that a pane was closed.
    PaneClosed {
        request_id: RequestId,
        pane_id: PaneId,
        exit_code: Option<i32>,
    },

    /// The client fell too far behind and missed output. Re-attach with `last_seqno`.
    Lagged { pane_id: PaneId, missed_count: u64 },

    /// Full snapshot was sent because the requested seqno is no longer in the buffer.
    SyncReset { pane_id: PaneId },

    /// Asynchronous lifecycle event.
    Event { event: SessionEventMsg },

    /// Structured error response.
    Error {
        request_id: Option<RequestId>,
        code: ErrorCode,
        message: String,
    },

    /// Server -> client keep-alive ping; client must reply with `Pong`.
    Ping { seq: u64 },

    /// Response to client `Ping`.
    Pong { seq: u64 },

    /// Server-side VT diff for an attached pane.
    TerminalUpdate {
        pane_id: PaneId,
        diff: Arc<TerminalDiff>,
        seqno: SequenceNo,
        /// Wall-clock timestamp (ms since Unix epoch) when the server sent this message.
        sent_at_ms: u64,
    },

    /// Full grid snapshot for an attached pane (sent on attach/resize).
    TerminalSnapshot {
        pane_id: PaneId,
        snapshot: GridSnapshot,
        seqno: SequenceNo,
        /// Wall-clock timestamp (ms since Unix epoch) when the server sent this message.
        sent_at_ms: u64,
    },

    /// Cursor-only update for an attached pane (no cell changes).
    ///
    /// Shares the same seqno space as `TerminalUpdate` so client gap
    /// detection works unchanged.
    CursorUpdate {
        pane_id: PaneId,
        cursor: CursorState,
        modes: TermModes,
        seqno: SequenceNo,
        sent_at_ms: u64,
    },

    /// Input lock granted to the requesting client.
    InputLockGranted { pane_id: PaneId },

    /// Input lock request denied; another client holds it.
    InputLockDenied { pane_id: PaneId, holder: ClientId },

    /// The input lock for a pane was released.
    InputLockReleased { pane_id: PaneId },

    /// Confirmation that a session was renamed.
    SessionRenamed { word_id: WordId, new_name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_attrs_bits_no_overlap() {
        let flags: &[u16] = &[
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::STRIKETHROUGH,
            CellAttrs::INVERSE,
            CellAttrs::HIDDEN,
            CellAttrs::DIM,
            CellAttrs::BLINK,
            CellAttrs::WIDE_CHAR,
            CellAttrs::WIDE_CHAR_SPACER,
        ];
        for (i, a) in flags.iter().enumerate() {
            for (j, b) in flags.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bit overlap between flag {i} and {j}");
                }
            }
        }
    }

    #[test]
    fn term_modes_bits_no_overlap() {
        let flags: &[u16] = &[
            TermModes::APP_CURSOR,
            TermModes::BRACKETED_PASTE,
            TermModes::MOUSE_REPORT_CLICK,
            TermModes::MOUSE_DRAG,
            TermModes::MOUSE_MOTION,
            TermModes::SGR_MOUSE,
        ];
        for (i, a) in flags.iter().enumerate() {
            assert!(a.is_power_of_two(), "flag {i} is not a single bit: {a}");
            for (j, b) in flags.iter().enumerate() {
                if i != j {
                    assert_eq!(a & b, 0, "bit overlap between flag {i} and {j}");
                }
            }
        }
    }

    #[test]
    fn term_modes_accessors() {
        let empty = TermModes::EMPTY;
        assert!(!empty.app_cursor());
        assert!(!empty.bracketed_paste());

        let bp = TermModes(TermModes::BRACKETED_PASTE);
        assert!(!bp.app_cursor());
        assert!(bp.bracketed_paste());

        let both = TermModes(TermModes::APP_CURSOR | TermModes::BRACKETED_PASTE);
        assert!(both.app_cursor());
        assert!(both.bracketed_paste());

        let mouse = TermModes(TermModes::MOUSE_REPORT_CLICK | TermModes::SGR_MOUSE);
        assert!(mouse.mouse_report());
        assert!(mouse.sgr_mouse());
        assert!(!mouse.app_cursor());

        let empty = TermModes::EMPTY;
        assert!(!empty.mouse_report());
        assert!(!empty.sgr_mouse());

        let drag = TermModes(TermModes::MOUSE_DRAG);
        assert!(drag.mouse_report());
        assert!(!drag.sgr_mouse());
    }

    #[test]
    fn cell_attrs_each_flag_is_single_bit() {
        let flags: &[u16] = &[
            CellAttrs::BOLD,
            CellAttrs::ITALIC,
            CellAttrs::UNDERLINE,
            CellAttrs::STRIKETHROUGH,
            CellAttrs::INVERSE,
            CellAttrs::HIDDEN,
            CellAttrs::DIM,
            CellAttrs::BLINK,
            CellAttrs::WIDE_CHAR,
            CellAttrs::WIDE_CHAR_SPACER,
        ];
        for (i, flag) in flags.iter().enumerate() {
            assert!(
                flag.is_power_of_two(),
                "flag {i} is not a single bit: {flag}"
            );
        }
    }

    #[test]
    fn pane_id_format() {
        let word_id = "eagle".to_string();
        let pane_index = 0u32;
        let pane_id = format!("{word_id}/{pane_index}");
        assert_eq!(pane_id, "eagle/0");

        // Parse back
        let (w, idx_str) = pane_id.rsplit_once('/').unwrap();
        let idx: u32 = idx_str.parse().unwrap();
        assert_eq!(w, "eagle");
        assert_eq!(idx, 0);
    }
}
