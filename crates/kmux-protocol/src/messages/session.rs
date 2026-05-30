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
/// emulator (currently libghostty-vt).
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

/// Terminal dimensions (rows × columns × optional pixel extent).
///
/// `pixel_width` and `pixel_height` represent the total drawable area of the
/// terminal window in physical pixels.  A value of `0` means the client does
/// not know (or the platform does not expose) the pixel dimensions — backends
/// must treat `0` as "unknown" and fall back to cell-only sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
    /// Total window width in physical pixels; `0` = unknown.
    pub pixel_width: u16,
    /// Total window height in physical pixels; `0` = unknown.
    pub pixel_height: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
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
    /// Latest window title reported by the pane's program via OSC 0/2.
    /// Empty until the program emits a title sequence.
    pub title: String,
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
    PaneResized { pane_id: PaneId, size: TermSize },
    /// A pane's program reported a new window title (OSC 0/2).
    PaneTitleChanged { pane_id: PaneId, title: String },
    /// A pane's program wrote the clipboard via OSC 52. `selection` is the
    /// normalized target ("c"/"p"/"s"/"0".."7"); `data` is the still
    /// base64-encoded payload (decoded client-side at the clipboard leaf).
    PaneClipboardCopy {
        pane_id: PaneId,
        selection: String,
        data: String,
    },
    /// A pane was closed.
    PaneClosed { pane_id: PaneId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_size_pixel_fields_roundtrip() {
        let size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 1920,
            pixel_height: 1080,
        };
        let bytes = postcard::to_allocvec(&size).expect("serialize");
        let decoded: TermSize = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.rows, 40);
        assert_eq!(decoded.cols, 120);
        assert_eq!(decoded.pixel_width, 1920);
        assert_eq!(decoded.pixel_height, 1080);
    }

    #[test]
    fn term_size_default_has_zero_pixel_dims() {
        let d = TermSize::default();
        assert_eq!(d.rows, 24);
        assert_eq!(d.cols, 80);
        assert_eq!(d.pixel_width, 0);
        assert_eq!(d.pixel_height, 0);
    }

    #[test]
    fn pane_title_changed_roundtrips() {
        let msg = SessionEventMsg::PaneTitleChanged {
            pane_id: "eagle/0".to_string(),
            title: "~/dev/kmux".to_string(),
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneTitleChanged { pane_id, title } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(title, "~/dev/kmux");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pane_clipboard_copy_roundtrips() {
        let msg = SessionEventMsg::PaneClipboardCopy {
            pane_id: "eagle/0".to_string(),
            selection: "c".to_string(),
            data: "aGVsbG8=".to_string(),
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneClipboardCopy {
                pane_id,
                selection,
                data,
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(selection, "c");
                assert_eq!(data, "aGVsbG8=");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pane_resized_carries_term_size() {
        let msg = SessionEventMsg::PaneResized {
            pane_id: "eagle/0".to_string(),
            size: TermSize {
                rows: 30,
                cols: 100,
                pixel_width: 1000,
                pixel_height: 600,
            },
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneResized { pane_id, size } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(size.rows, 30);
                assert_eq!(size.pixel_width, 1000);
            }
            _ => panic!("wrong variant"),
        }
    }
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
