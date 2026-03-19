use serde::{Deserialize, Serialize};

/// Current wire protocol version. Increment when breaking changes are made.
pub const PROTOCOL_VERSION: u32 = 2;

pub type SessionId = String;
pub type RequestId = u64;

/// Opaque client identity assigned by the server on successful authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub u64);

/// Monotonic sequence number attached to each PTY output chunk per session.
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

/// Snapshot of a session as reported by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: SessionId,
    pub program: String,
    pub size: TermSize,
    /// IDs of currently attached clients.
    pub attached_clients: Vec<ClientId>,
    /// Whether the session's child process is still running.
    pub status: SessionStatus,
}

/// Input control mode for a session.
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
    Spawned {
        name: SessionId,
    },
    Exited {
        name: SessionId,
        code: Option<i32>,
        signal: Option<i32>,
    },
    Resized {
        name: SessionId,
        rows: u16,
        cols: u16,
    },
    Closed {
        name: SessionId,
    },
    /// Session was renamed.
    Renamed {
        old_name: SessionId,
        new_name: SessionId,
    },
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

    /// Request creation of a new named PTY session.
    SessionCreate {
        request_id: RequestId,
        name: SessionId,
        /// Shell or program to run; defaults to system shell if `None`.
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    },

    /// Request graceful close of a session.
    SessionClose {
        request_id: RequestId,
        name: SessionId,
    },

    /// Request a list of all active sessions.
    SessionList { request_id: RequestId },

    /// Rename an existing session.
    SessionRename {
        request_id: RequestId,
        session: SessionId,
        new_name: String,
    },

    /// Send bytes to the PTY master (user keystrokes, paste, etc.).
    PtyInput { session: SessionId, data: Vec<u8> },

    /// Resize the PTY window.
    Resize { session: SessionId, size: TermSize },

    /// Subscribe to PTY output for a session.
    ///
    /// `last_seqno = None`       -> send full snapshot (first attach or full resync)
    /// `last_seqno = Some(n)`    -> replay only chunks with seqno > n (reconnect)
    Attach {
        session: SessionId,
        last_seqno: Option<SequenceNo>,
    },

    /// Unsubscribe from PTY output for a session.
    Detach { session: SessionId },

    /// Send a Unix signal to the PTY child process.
    Signal { session: SessionId, signal: i32 },

    /// Request exclusive input rights for a session.
    RequestInputLock { session: SessionId },

    /// Release previously acquired input lock.
    ReleaseInputLock { session: SessionId },

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
    },

    /// Confirmation that a session was created.
    SessionCreated {
        request_id: RequestId,
        name: SessionId,
    },

    /// Confirmation that a session was closed.
    SessionClosed {
        request_id: RequestId,
        name: SessionId,
        exit_code: Option<i32>,
    },

    /// Response to `SessionList`.
    SessionListResult {
        request_id: RequestId,
        sessions: Vec<SessionInfo>,
    },

    /// PTY output chunk for an attached session, tagged with a sequence number.
    PtyOutput {
        session: SessionId,
        data: Vec<u8>,
        seqno: SequenceNo,
    },

    /// The client fell too far behind and missed output. Re-attach with `last_seqno`.
    Lagged {
        session: SessionId,
        missed_count: u64,
    },

    /// Full snapshot was sent because the requested seqno is no longer in the buffer.
    SyncReset { session: SessionId },

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

    /// Input lock granted to the requesting client.
    InputLockGranted { session: SessionId },

    /// Input lock request denied; another client holds it.
    InputLockDenied {
        session: SessionId,
        holder: ClientId,
    },

    /// The input lock for a session was released.
    InputLockReleased { session: SessionId },

    /// Confirmation that a session was renamed.
    SessionRenamed {
        old_name: SessionId,
        new_name: SessionId,
    },
}
