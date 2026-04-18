use super::session::{
    ClientCapabilities, ConnectionId, PaneId, RequestId, SequenceNo, TermSize, WordId,
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

    /// Keep-alive ping (client -> server).
    Ping { seq: u64 },

    /// Response to server Ping.
    Pong { seq: u64 },
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
}
