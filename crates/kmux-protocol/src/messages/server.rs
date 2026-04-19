use std::sync::Arc;

use super::session::{
    ClientId, ConnectionId, ErrorCode, PaneId, RequestId, SequenceNo, SessionEntry,
    SessionEventMsg, WordId,
};
use super::vt::{CellState, CursorState, GridSnapshot, TermModes, TerminalDiff};

/// Messages sent from server -> client.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ServerMessage {
    /// Response to `Auth`.
    AuthResult {
        success: bool,
        reason: Option<String>,
        /// Assigned on success; `None` on failure.
        client_id: Option<ClientId>,
        /// Server binary version (e.g. `"0.1.0"`); `None` on failure.
        server_version: Option<String>,
        /// Connection identity assigned (or reconfirmed) by the server.
        /// Always `Some` on success. The client must store this and pass it
        /// when re-authenticating on a new transport channel.
        #[serde(default)]
        connection_id: Option<ConnectionId>,
    },

    /// Confirmation that the channel switch is complete. Sent in response to
    /// `ChannelReady` on the new transport. The client should close the old
    /// transport after receiving this.
    ChannelSwitched {
        /// Human-readable name of the transport that was replaced ("quic" or "tcp").
        old_transport: String,
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
        /// Terminal dimensions the server assigned to the new pane.
        size: super::session::TermSize,
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

    /// Notification that `lines` have been appended to the daemon's
    /// scrollback mirror for this pane, starting at absolute index
    /// `first_index`. Shares seqno space with `TerminalUpdate` so the client
    /// can detect gaps and issue `FetchHistory` for anything it missed.
    ScrollbackAppend {
        pane_id: PaneId,
        /// Absolute index of the first line in `lines`.
        first_index: u64,
        /// Appended lines, oldest first. Each line is stored at the column
        /// width it had when captured.
        lines: Vec<Vec<CellState>>,
        seqno: SequenceNo,
        sent_at_ms: u64,
    },

    /// Response to `FetchHistory`. Returns `lines` starting at absolute
    /// index `first_index`. `history_total` echoes the mirror's current size
    /// so the client can detect eviction since it issued the request.
    HistoryLines {
        request_id: RequestId,
        pane_id: PaneId,
        /// Absolute index of the first returned line.
        first_index: u64,
        lines: Vec<Vec<CellState>>,
        /// Mirror's absolute line count at reply time. The oldest available
        /// line is at `history_total - mirror_capacity` (conceptually).
        history_total: u64,
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
