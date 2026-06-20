use std::sync::Arc;

use super::category::MessageCategory;
use super::process::PaneProcesses;
use super::session::{
    ClientId, ConnectionId, DirEntry, ErrorCode, LayoutNode, PaneId, PaneInfo, PeerId, RequestId,
    SequenceNo, SessionEntry, SessionEventMsg, TabIndex, TabInfo, WordId,
};
use super::types::Compression;
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
        /// Compression the daemon chose for this connection (HTTP
        /// `Content-Encoding` analogue). `None` = uncompressed. The daemon
        /// decides based on client locality and config; the client only needs
        /// this for observability since `read_frame` decompresses per-frame
        /// regardless. See `docs/compression.md`.
        #[serde(default)]
        compression: Option<Compression>,
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

    /// Response to `ProcessOverview` (issue #122): the process tree of every
    /// pane (local plus every federated peer's), keyed by local pane id.
    ProcessOverviewResult {
        request_id: RequestId,
        panes: Vec<PaneProcesses>,
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

    /// Confirmation that a new tab was created.
    TabCreated {
        request_id: RequestId,
        word_id: WordId,
        tab: TabInfo,
    },

    /// Confirmation that a tab was closed.
    TabClosed {
        request_id: RequestId,
        word_id: WordId,
        tab_index: TabIndex,
    },

    /// Confirmation that a pane was split: carries the freshly spawned pane plus
    /// the tab's new authoritative layout tree.
    PaneSplit {
        request_id: RequestId,
        word_id: WordId,
        tab_index: TabIndex,
        new_pane: PaneInfo,
        layout: LayoutNode,
    },

    /// Authoritative layout state for one tab. Broadcast to every client viewing
    /// the tab after **any** layout mutation (split, close, swap, resize, focus)
    /// and sent on (re)attach. Clients replace their cached tree wholesale — they
    /// never merge — so concurrent client edits resolve last-writer-wins.
    LayoutUpdate {
        word_id: WordId,
        tab_index: TabIndex,
        layout: LayoutNode,
        focused_pane: u32,
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

    /// Response to [`super::client::ClientMessage::ListDirectory`]. Carries the
    /// daemon host's directory listing the client browses to choose where a new
    /// session is created.
    DirectoryListing {
        request_id: RequestId,
        /// The canonicalized directory actually listed (echoes the requested
        /// path on error so the client can keep showing where it tried to go).
        path: String,
        /// The listed directory's parent, or `None` at the filesystem root.
        parent: Option<String>,
        /// The listed directory's subdirectories only (we are choosing a
        /// directory). Empty when `error` is `Some`.
        entries: Vec<DirEntry>,
        /// A human-readable message when the listing failed (e.g. permission
        /// denied); `None` on success.
        error: Option<String>,
    },

    /// Acknowledges a successful [`super::client::ClientMessage::OpenPeer`]
    /// (issue #121). The peer's sessions arrive via the normal session-list
    /// flow (a follow-up `SessionListResult` / `Event`s), each under a
    /// locally-assigned `word_id`; `peer` echoes the federated peer's id so the
    /// client can label them.
    PeerOpened { request_id: RequestId, peer: PeerId },

    /// Acknowledges a [`super::client::ClientMessage::ClosePeer`]: the peer's
    /// sessions have been removed from this client's list.
    PeerClosed { request_id: RequestId, peer: PeerId },

    /// A federation request failed (could not reach or authenticate the peer,
    /// or a protocol-version mismatch). `peer` is the target id when known.
    PeerError {
        request_id: RequestId,
        peer: Option<PeerId>,
        reason: String,
    },
}

impl ServerMessage {
    /// Classify this message into a [`MessageCategory`] for metrics attribution.
    /// The match is exhaustive — adding a new variant without updating this
    /// function is a compile error.
    pub fn category(&self) -> MessageCategory {
        match self {
            Self::TerminalUpdate { .. }
            | Self::TerminalSnapshot { .. }
            | Self::CursorUpdate { .. } => MessageCategory::Shell,
            Self::ScrollbackAppend { .. } | Self::HistoryLines { .. } => {
                MessageCategory::Scrollback
            }
            Self::Ping { .. } | Self::Pong { .. } => MessageCategory::Liveness,
            Self::SessionCreated { .. }
            | Self::SessionClosed { .. }
            | Self::SessionListResult { .. }
            | Self::ProcessOverviewResult { .. }
            | Self::SessionRenamed { .. }
            | Self::PaneCreated { .. }
            | Self::PaneClosed { .. }
            | Self::TabCreated { .. }
            | Self::TabClosed { .. }
            | Self::PaneSplit { .. }
            | Self::LayoutUpdate { .. }
            | Self::Event { .. }
            | Self::Error { .. }
            | Self::InputLockGranted { .. }
            | Self::InputLockDenied { .. }
            | Self::InputLockReleased { .. }
            | Self::DirectoryListing { .. }
            | Self::PeerOpened { .. }
            | Self::PeerClosed { .. }
            | Self::PeerError { .. } => MessageCategory::Control,
            Self::Lagged { .. } | Self::SyncReset { .. } => MessageCategory::Sync,
            Self::AuthResult { .. } | Self::ChannelSwitched { .. } => MessageCategory::Bootstrap,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::session::{
        ClientId, ErrorCode, PaneInfo, SessionEntry, SessionEventMsg, SessionMeta, TermSize,
    };
    use super::super::vt::{CursorState, GridSnapshot, TermModes, TerminalDiff};
    use super::*;

    fn dummy_session_entry() -> SessionEntry {
        use super::super::session::{LayoutNode, TabInfo};
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "w".into(),
                name: "n".into(),
                cwd: "/".into(),
            },
            panes: vec![],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".into(),
                layout: LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        }
    }

    fn dummy_terminal_diff() -> TerminalDiff {
        TerminalDiff {
            ops: vec![],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        }
    }

    fn dummy_grid_snapshot() -> GridSnapshot {
        GridSnapshot {
            rows: 24,
            cols: 80,
            cells: vec![],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: vec![],
        }
    }

    #[test]
    fn category_covers_every_server_variant() {
        let cases: Vec<(ServerMessage, MessageCategory)> = vec![
            (
                ServerMessage::TerminalUpdate {
                    pane_id: "p".into(),
                    diff: Arc::new(dummy_terminal_diff()),
                    seqno: SequenceNo(1),
                    sent_at_ms: 0,
                },
                MessageCategory::Shell,
            ),
            (
                ServerMessage::TerminalSnapshot {
                    pane_id: "p".into(),
                    snapshot: dummy_grid_snapshot(),
                    seqno: SequenceNo(1),
                    sent_at_ms: 0,
                },
                MessageCategory::Shell,
            ),
            (
                ServerMessage::CursorUpdate {
                    pane_id: "p".into(),
                    cursor: CursorState::default(),
                    modes: TermModes::EMPTY,
                    seqno: SequenceNo(1),
                    sent_at_ms: 0,
                },
                MessageCategory::Shell,
            ),
            (
                ServerMessage::ScrollbackAppend {
                    pane_id: "p".into(),
                    first_index: 0,
                    lines: vec![],
                    seqno: SequenceNo(1),
                    sent_at_ms: 0,
                },
                MessageCategory::Scrollback,
            ),
            (
                ServerMessage::HistoryLines {
                    request_id: 0,
                    pane_id: "p".into(),
                    first_index: 0,
                    lines: vec![],
                    history_total: 0,
                    sent_at_ms: 0,
                },
                MessageCategory::Scrollback,
            ),
            (ServerMessage::Ping { seq: 1 }, MessageCategory::Liveness),
            (ServerMessage::Pong { seq: 1 }, MessageCategory::Liveness),
            (
                ServerMessage::SessionCreated {
                    request_id: 0,
                    entry: dummy_session_entry(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::SessionClosed {
                    request_id: 0,
                    word_id: "w".into(),
                    exit_code: None,
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::SessionListResult {
                    request_id: 0,
                    sessions: vec![],
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::ProcessOverviewResult {
                    request_id: 0,
                    panes: vec![],
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::SessionRenamed {
                    word_id: "w".into(),
                    new_name: "n".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PaneCreated {
                    request_id: 0,
                    pane_id: "p".into(),
                    session_word_id: "w".into(),
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PaneClosed {
                    request_id: 0,
                    pane_id: "p".into(),
                    exit_code: None,
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::TabCreated {
                    request_id: 0,
                    word_id: "w".into(),
                    tab: super::super::session::TabInfo {
                        tab_index: 0,
                        name: "1".into(),
                        layout: super::super::session::LayoutNode::single(0),
                        focused_pane: 0,
                    },
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::TabClosed {
                    request_id: 0,
                    word_id: "w".into(),
                    tab_index: 0,
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PaneSplit {
                    request_id: 0,
                    word_id: "w".into(),
                    tab_index: 0,
                    new_pane: PaneInfo {
                        pane_id: "w/1".into(),
                        pane_index: 1,
                        program: "/bin/sh".into(),
                        size: TermSize::default(),
                        attached_clients: vec![],
                        status: super::super::session::SessionStatus::Running,
                        title: String::new(),
                        progress_state: Default::default(),
                        progress: None,
                    },
                    layout: super::super::session::LayoutNode::single(0),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::LayoutUpdate {
                    word_id: "w".into(),
                    tab_index: 0,
                    layout: super::super::session::LayoutNode::single(0),
                    focused_pane: 0,
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::Event {
                    event: SessionEventMsg::SessionCreated {
                        word_id: "w".into(),
                    },
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::Error {
                    request_id: None,
                    code: ErrorCode::InternalError,
                    message: "e".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::InputLockGranted {
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::InputLockDenied {
                    pane_id: "p".into(),
                    holder: ClientId(1),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::InputLockReleased {
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::Lagged {
                    pane_id: "p".into(),
                    missed_count: 1,
                },
                MessageCategory::Sync,
            ),
            (
                ServerMessage::SyncReset {
                    pane_id: "p".into(),
                },
                MessageCategory::Sync,
            ),
            (
                ServerMessage::AuthResult {
                    success: true,
                    reason: None,
                    client_id: None,
                    server_version: None,
                    connection_id: None,
                    compression: None,
                },
                MessageCategory::Bootstrap,
            ),
            (
                ServerMessage::ChannelSwitched {
                    old_transport: "tcp".into(),
                },
                MessageCategory::Bootstrap,
            ),
            (
                ServerMessage::DirectoryListing {
                    request_id: 0,
                    path: "/tmp".into(),
                    parent: Some("/".into()),
                    entries: vec![],
                    error: None,
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PeerOpened {
                    request_id: 0,
                    peer: "box".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PeerClosed {
                    request_id: 0,
                    peer: "box".into(),
                },
                MessageCategory::Control,
            ),
            (
                ServerMessage::PeerError {
                    request_id: 0,
                    peer: Some("box".into()),
                    reason: "nope".into(),
                },
                MessageCategory::Control,
            ),
        ];
        for (msg, expected) in &cases {
            assert_eq!(msg.category(), *expected, "wrong category for {msg:?}");
        }
    }

    #[test]
    fn directory_listing_roundtrips() {
        use super::super::session::DirEntry;
        let msg = ServerMessage::DirectoryListing {
            request_id: 5,
            path: "/home/user".into(),
            parent: Some("/home".into()),
            entries: vec![
                DirEntry {
                    name: "dev".into(),
                    is_dir: true,
                },
                DirEntry {
                    name: "docs".into(),
                    is_dir: true,
                },
            ],
            error: None,
        };
        let bytes = crate::encode_server(&msg).unwrap();
        match crate::decode_server(&bytes).unwrap() {
            ServerMessage::DirectoryListing {
                request_id,
                path,
                parent,
                entries,
                error,
            } => {
                assert_eq!(request_id, 5);
                assert_eq!(path, "/home/user");
                assert_eq!(parent.as_deref(), Some("/home"));
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].name, "dev");
                assert!(entries[0].is_dir);
                assert!(error.is_none());
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn directory_listing_error_roundtrips() {
        let msg = ServerMessage::DirectoryListing {
            request_id: 1,
            path: "/root/secret".into(),
            parent: None,
            entries: vec![],
            error: Some("Permission denied (os error 13)".into()),
        };
        let bytes = crate::encode_server(&msg).unwrap();
        match crate::decode_server(&bytes).unwrap() {
            ServerMessage::DirectoryListing {
                parent,
                entries,
                error,
                ..
            } => {
                assert!(parent.is_none());
                assert!(entries.is_empty());
                assert!(error.unwrap().contains("Permission denied"));
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn peer_messages_roundtrip() {
        let opened = ServerMessage::PeerOpened {
            request_id: 3,
            peer: "alice@box".into(),
        };
        let bytes = crate::encode_server(&opened).unwrap();
        match crate::decode_server(&bytes).unwrap() {
            ServerMessage::PeerOpened { request_id, peer } => {
                assert_eq!(request_id, 3);
                assert_eq!(peer, "alice@box");
            }
            other => panic!("expected PeerOpened, got {other:?}"),
        }

        let err = ServerMessage::PeerError {
            request_id: 4,
            peer: Some("alice@box".into()),
            reason: "version mismatch".into(),
        };
        let bytes = crate::encode_server(&err).unwrap();
        match crate::decode_server(&bytes).unwrap() {
            ServerMessage::PeerError {
                request_id,
                peer,
                reason,
            } => {
                assert_eq!(request_id, 4);
                assert_eq!(peer.as_deref(), Some("alice@box"));
                assert_eq!(reason, "version mismatch");
            }
            other => panic!("expected PeerError, got {other:?}"),
        }
    }
}
