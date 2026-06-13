use super::category::MessageCategory;
use super::key::KeyEvent;
use super::session::{
    ClientCapabilities, ConnectionId, LayoutScheme, PaneId, RequestId, SequenceNo, SplitDir,
    TabIndex, TermSize, WordId,
};

/// Messages sent from client -> server.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum ClientMessage {
    /// First message: authenticate with a shared token and declare capabilities.
    Auth {
        token: String,
        /// Must equal `PROTOCOL_VERSION`; server rejects mismatches.
        protocol_version: u32,
        /// Rendering capabilities of this client.  The daemon uses these to
        /// set an appropriate shell environment and to configure the
        /// server-side VT emulator feature flags for each pane.
        capabilities: ClientCapabilities,
        /// When switching transports (QUIC ↔ TCP), pass the existing
        /// `ConnectionId` to resume the session on the new channel.
        /// `None` for a fresh connection.
        #[serde(default)]
        connection_id: Option<ConnectionId>,
    },

    /// Signal to the server that this channel is ready to become the primary
    /// transport. Sent after a successful channel-switch `Auth`. The server
    /// responds with `ChannelSwitched` and then closes the old channel.
    ChannelReady,

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

    /// Request graceful close of a single pane. The server removes the pane's
    /// leaf from its tab's layout tree (collapsing the parent split) and
    /// broadcasts a `LayoutUpdate` for the affected tab.
    PaneClose {
        request_id: RequestId,
        pane_id: PaneId,
    },

    /// Create a new tab inside an existing session, with one fresh pane.
    /// (This is what the user-facing "new tab" action does; the previous
    /// "new pane" semantics map here.)
    TabCreate {
        request_id: RequestId,
        word_id: WordId,
        /// Shell or program to run in the tab's initial pane; defaults to the
        /// system shell if `None`.
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    },

    /// Request graceful close of an entire tab and every pane unique to it.
    TabClose {
        request_id: RequestId,
        word_id: WordId,
        tab_index: TabIndex,
    },

    /// Rename a tab's display name.
    TabRename {
        request_id: RequestId,
        word_id: WordId,
        tab_index: TabIndex,
        new_name: String,
    },

    /// Split the focused (or named) pane within a tab, spawning a new pane (PTY)
    /// adjacent to it in `dir`. The server spawns the PTY, inserts its leaf into
    /// the tab's layout tree, and broadcasts the new tree via `PaneSplit` /
    /// `LayoutUpdate`.
    PaneSplit {
        request_id: RequestId,
        word_id: WordId,
        tab_index: TabIndex,
        /// `pane_index` of the leaf to split.
        from_pane: u32,
        dir: SplitDir,
        /// Shell or program for the new pane; defaults to the system shell.
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    },

    /// Swap two panes' positions within a tab's layout (exchange the two leaves
    /// in place; split ratios are untouched).
    PaneSwap {
        word_id: WordId,
        tab_index: TabIndex,
        a: u32,
        b: u32,
    },

    /// Adjust the child weights of one `Split` node, addressed by `path`
    /// (child-index descent from the tab's layout root). Used to resize a split.
    /// The server clamps to minimum sizes, renormalizes to 1000, and broadcasts.
    SetLayoutRatios {
        word_id: WordId,
        tab_index: TabIndex,
        path: Vec<u32>,
        ratios: Vec<u16>,
    },

    /// Regenerate a tab's layout tree into a preset arrangement (tmux-style) from
    /// its current panes. The server rebuilds the tree and broadcasts the result.
    ApplyLayoutScheme {
        word_id: WordId,
        tab_index: TabIndex,
        scheme: LayoutScheme,
    },

    /// Set which pane has input focus within a tab (the shared, server-tracked
    /// focus). Broadcast to all clients viewing the tab.
    SetFocus {
        word_id: WordId,
        tab_index: TabIndex,
        pane_index: u32,
    },

    /// Send bytes to the PTY master (user keystrokes).
    PtyInput { pane_id: PaneId, data: Vec<u8> },

    /// Send a structured key event to a pane.  The daemon encodes it via
    /// the pane's live Ghostty key encoder, which tracks DECCKM, kitty
    /// keyboard flags, modifyOtherKeys, etc., so the bytes always match
    /// what the inner program negotiated.
    ///
    /// Use this for genuine keystrokes; use `PtyInput` for raw byte writes
    /// (paste, mouse-report scroll wheels, signal injection).
    PtyKey { pane_id: PaneId, event: KeyEvent },

    /// Batch of structured key events for one pane, encoded by the daemon
    /// in order before being written to the PTY.  Lets the client coalesce
    /// rapid keystrokes (e.g. typing through autocomplete) into a single
    /// message without losing per-event state for encoding.
    PtyKeyBatch {
        pane_id: PaneId,
        events: Vec<KeyEvent>,
    },

    /// Paste clipboard text into the PTY. The server handles bracketed-paste
    /// wrapping when the terminal has enabled DEC private mode 2004.
    PtyPaste { pane_id: PaneId, data: String },

    /// Resize the PTY window.
    Resize { pane_id: PaneId, size: TermSize },

    /// Subscribe to PTY output for a pane.
    ///
    /// `last_seqno = None`       -> send full snapshot (first attach or full resync)
    /// `last_seqno = Some(n)`    -> replay only chunks with seqno > n (reconnect)
    ///
    /// `size` is the client's current terminal size.  The server uses this to
    /// compute the effective pane size across all attached clients (smallest-wins).
    Attach {
        pane_id: PaneId,
        last_seqno: Option<SequenceNo>,
        /// Client's current terminal dimensions.
        size: TermSize,
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

    /// Ask the daemon for a range of scrollback lines starting at the given
    /// absolute index. Used to fill gaps (missed `ScrollbackAppend` frames)
    /// or lazily hydrate older history when the user scrolls past the
    /// client's cache.
    FetchHistory {
        request_id: RequestId,
        pane_id: PaneId,
        /// Absolute index of the first requested line.
        start_index: u64,
        /// Maximum number of lines to return.
        count: u32,
    },

    /// Keep-alive ping (client -> server).
    Ping { seq: u64 },

    /// Response to server Ping.
    Pong { seq: u64 },

    /// Request a listing of the daemon host's directories under `path`, so the
    /// client can browse the (possibly remote) filesystem to pick where a new
    /// session should be created. An empty `path` asks the daemon to resolve a
    /// sensible default (the user's home directory, else `"."`). The daemon
    /// replies with [`super::server::ServerMessage::DirectoryListing`].
    ListDirectory { request_id: RequestId, path: String },
}

impl ClientMessage {
    /// Classify this message into a [`MessageCategory`] for metrics attribution.
    /// The match is exhaustive — adding a new variant without updating this
    /// function is a compile error.
    pub fn category(&self) -> MessageCategory {
        match self {
            Self::PtyInput { .. }
            | Self::PtyPaste { .. }
            | Self::PtyKey { .. }
            | Self::PtyKeyBatch { .. } => MessageCategory::Shell,
            Self::FetchHistory { .. } => MessageCategory::Scrollback,
            Self::Ping { .. } | Self::Pong { .. } => MessageCategory::Liveness,
            Self::SessionCreate { .. }
            | Self::SessionClose { .. }
            | Self::SessionList { .. }
            | Self::SessionRename { .. }
            | Self::PaneCreate { .. }
            | Self::PaneClose { .. }
            | Self::TabCreate { .. }
            | Self::TabClose { .. }
            | Self::TabRename { .. }
            | Self::PaneSplit { .. }
            | Self::PaneSwap { .. }
            | Self::SetLayoutRatios { .. }
            | Self::ApplyLayoutScheme { .. }
            | Self::SetFocus { .. }
            | Self::Resize { .. }
            | Self::Attach { .. }
            | Self::Detach { .. }
            | Self::Signal { .. }
            | Self::RequestInputLock { .. }
            | Self::ReleaseInputLock { .. }
            | Self::SetSnapshotMode { .. }
            | Self::ListDirectory { .. } => MessageCategory::Control,
            Self::Auth { .. } | Self::ChannelReady => MessageCategory::Bootstrap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::server::ServerMessage;
    use super::super::session::ConnectionId;
    use super::super::types::{PROTOCOL_VERSION, version_mismatch_hint};
    use super::*;

    #[test]
    fn auth_message_roundtrip_with_connection_id() {
        let msg = ClientMessage::Auth {
            token: "tok".to_string(),
            protocol_version: PROTOCOL_VERSION,
            capabilities: ClientCapabilities::default(),
            connection_id: Some(ConnectionId(42)),
        };
        let bytes = crate::encode_client(&msg).unwrap();
        let decoded = crate::decode_client(&bytes).unwrap();
        assert!(matches!(
            decoded,
            ClientMessage::Auth {
                connection_id: Some(ConnectionId(42)),
                ..
            }
        ));
    }

    #[test]
    fn auth_result_roundtrip_with_connection_id() {
        let msg = ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: None,
            server_version: None,
            connection_id: Some(ConnectionId(99)),
            compression: None,
        };
        let bytes = crate::encode_server(&msg).unwrap();
        let decoded = crate::decode_server(&bytes).unwrap();
        assert!(matches!(
            decoded,
            ServerMessage::AuthResult {
                connection_id: Some(ConnectionId(99)),
                ..
            }
        ));
    }

    #[test]
    fn channel_ready_and_switched_roundtrip() {
        let ready = ClientMessage::ChannelReady;
        let bytes = crate::encode_client(&ready).unwrap();
        assert!(matches!(
            crate::decode_client(&bytes).unwrap(),
            ClientMessage::ChannelReady
        ));

        let switched = ServerMessage::ChannelSwitched {
            old_transport: "tcp".to_string(),
        };
        let bytes = crate::encode_server(&switched).unwrap();
        assert!(matches!(
            crate::decode_server(&bytes).unwrap(),
            ServerMessage::ChannelSwitched { .. }
        ));
    }

    #[test]
    fn version_mismatch_auth_result_roundtrip() {
        let msg = ServerMessage::AuthResult {
            success: false,
            reason: Some("protocol version mismatch: client=12, server=13".to_string()),
            client_id: None,
            server_version: Some("0.1.0".to_string()),
            connection_id: None,
            compression: None,
        };
        let bytes = crate::encode_server(&msg).unwrap();
        let decoded = crate::decode_server(&bytes).unwrap();
        match decoded {
            ServerMessage::AuthResult {
                success,
                reason,
                server_version,
                ..
            } => {
                assert!(!success);
                assert_eq!(
                    reason.as_deref(),
                    Some("protocol version mismatch: client=12, server=13")
                );
                assert_eq!(server_version.as_deref(), Some("0.1.0"));
            }
            _ => panic!("expected AuthResult"),
        }
    }

    #[test]
    fn version_mismatch_hint_older_client() {
        let hint = version_mismatch_hint("protocol version mismatch: client=12, server=13");
        assert!(hint.contains("client is older"));
    }

    #[test]
    fn version_mismatch_hint_newer_client() {
        let hint = version_mismatch_hint("protocol version mismatch: client=14, server=13");
        assert!(hint.contains("client is newer"));
    }

    #[test]
    fn version_mismatch_hint_not_a_mismatch() {
        let hint = version_mismatch_hint("invalid token");
        assert!(hint.is_empty());
    }

    #[test]
    fn apply_layout_scheme_roundtrips() {
        use super::super::session::LayoutScheme;
        let msg = ClientMessage::ApplyLayoutScheme {
            word_id: "eagle".into(),
            tab_index: 2,
            scheme: LayoutScheme::MainHorizontal,
        };
        let bytes = crate::encode_client(&msg).unwrap();
        match crate::decode_client(&bytes).unwrap() {
            ClientMessage::ApplyLayoutScheme {
                word_id,
                tab_index,
                scheme,
            } => {
                assert_eq!(word_id, "eagle");
                assert_eq!(tab_index, 2);
                assert_eq!(scheme, LayoutScheme::MainHorizontal);
            }
            other => panic!("expected ApplyLayoutScheme, got {other:?}"),
        }
    }

    #[test]
    fn category_covers_every_client_variant() {
        use super::super::key::{KeyAction, KeyCode, KeyEvent, KeyMods};
        use super::super::session::TermSize;
        // One representative per variant — ensures the exhaustive match compiles
        // and each variant maps to a specific category.
        let cases: &[(ClientMessage, MessageCategory)] = &[
            (
                ClientMessage::PtyInput {
                    pane_id: "p".into(),
                    data: vec![b'a'],
                },
                MessageCategory::Shell,
            ),
            (
                ClientMessage::PtyPaste {
                    pane_id: "p".into(),
                    data: "x".into(),
                },
                MessageCategory::Shell,
            ),
            (
                ClientMessage::PtyKey {
                    pane_id: "p".into(),
                    event: KeyEvent {
                        code: KeyCode::Enter,
                        mods: KeyMods::SHIFT,
                        action: KeyAction::Press,
                        text: String::new(),
                        unshifted_codepoint: 0,
                    },
                },
                MessageCategory::Shell,
            ),
            (
                ClientMessage::PtyKeyBatch {
                    pane_id: "p".into(),
                    events: vec![],
                },
                MessageCategory::Shell,
            ),
            (
                ClientMessage::FetchHistory {
                    request_id: 0,
                    pane_id: "p".into(),
                    start_index: 0,
                    count: 10,
                },
                MessageCategory::Scrollback,
            ),
            (ClientMessage::Ping { seq: 1 }, MessageCategory::Liveness),
            (ClientMessage::Pong { seq: 1 }, MessageCategory::Liveness),
            (
                ClientMessage::SessionCreate {
                    request_id: 0,
                    name: None,
                    cwd: None,
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SessionClose {
                    request_id: 0,
                    word_id: "w".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SessionList { request_id: 0 },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SessionRename {
                    request_id: 0,
                    word_id: "w".into(),
                    new_name: "n".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::PaneCreate {
                    request_id: 0,
                    word_id: "w".into(),
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::PaneClose {
                    request_id: 0,
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::Resize {
                    pane_id: "p".into(),
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::Attach {
                    pane_id: "p".into(),
                    last_seqno: None,
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::Detach {
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::Signal {
                    pane_id: "p".into(),
                    signal: 15,
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::RequestInputLock {
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::ReleaseInputLock {
                    pane_id: "p".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SetSnapshotMode { enabled: false },
                MessageCategory::Control,
            ),
            (
                ClientMessage::TabCreate {
                    request_id: 0,
                    word_id: "w".into(),
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::TabClose {
                    request_id: 0,
                    word_id: "w".into(),
                    tab_index: 0,
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::TabRename {
                    request_id: 0,
                    word_id: "w".into(),
                    tab_index: 0,
                    new_name: "n".into(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::PaneSplit {
                    request_id: 0,
                    word_id: "w".into(),
                    tab_index: 0,
                    from_pane: 0,
                    dir: super::super::session::SplitDir::Horizontal,
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::PaneSwap {
                    word_id: "w".into(),
                    tab_index: 0,
                    a: 0,
                    b: 1,
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SetLayoutRatios {
                    word_id: "w".into(),
                    tab_index: 0,
                    path: vec![],
                    ratios: vec![500, 500],
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::ApplyLayoutScheme {
                    word_id: "w".into(),
                    tab_index: 0,
                    scheme: super::super::session::LayoutScheme::MainVertical,
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::SetFocus {
                    word_id: "w".into(),
                    tab_index: 0,
                    pane_index: 0,
                },
                MessageCategory::Control,
            ),
            (
                ClientMessage::Auth {
                    token: "t".into(),
                    protocol_version: PROTOCOL_VERSION,
                    capabilities: ClientCapabilities::default(),
                    connection_id: None,
                },
                MessageCategory::Bootstrap,
            ),
            (ClientMessage::ChannelReady, MessageCategory::Bootstrap),
            (
                ClientMessage::ListDirectory {
                    request_id: 0,
                    path: "/tmp".into(),
                },
                MessageCategory::Control,
            ),
        ];
        for (msg, expected) in cases {
            assert_eq!(msg.category(), *expected, "wrong category for {msg:?}");
        }
    }

    #[test]
    fn list_directory_roundtrips() {
        let msg = ClientMessage::ListDirectory {
            request_id: 7,
            path: "/home/user/dev".into(),
        };
        let bytes = crate::encode_client(&msg).unwrap();
        match crate::decode_client(&bytes).unwrap() {
            ClientMessage::ListDirectory { request_id, path } => {
                assert_eq!(request_id, 7);
                assert_eq!(path, "/home/user/dev");
            }
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }
}
