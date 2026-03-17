use serde::{Deserialize, Serialize};

pub type SessionId = String;
pub type RequestId = u64;

/// Terminal dimensions (rows × columns).
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

/// Snapshot of a session as reported by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: SessionId,
    pub program: String,
    pub size: TermSize,
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
}

/// Messages sent from client → server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message: authenticate with a shared token.
    Auth { token: String },

    /// Request creation of a new named PTY session.
    SessionCreate {
        request_id: RequestId,
        name: SessionId,
        /// Shell or program to run; defaults to `/bin/bash` if `None`.
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

    /// Send bytes to the PTY master (user keystrokes, paste, etc.).
    PtyInput { session: SessionId, data: Vec<u8> },

    /// Resize the PTY window.
    Resize { session: SessionId, size: TermSize },

    /// Subscribe to PTY output for a session (server starts forwarding output).
    Attach { session: SessionId },

    /// Unsubscribe from PTY output for a session.
    Detach { session: SessionId },

    /// Send a Unix signal to the PTY child process.
    Signal { session: SessionId, signal: i32 },

    /// Keep-alive ping.
    Ping { seq: u64 },
}

/// Messages sent from server → client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Response to `Auth`.
    AuthResult {
        success: bool,
        reason: Option<String>,
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

    /// PTY output chunk for an attached session.
    PtyOutput { session: SessionId, data: Vec<u8> },

    /// Asynchronous lifecycle event.
    Event { event: SessionEventMsg },

    /// Structured error response.
    Error {
        request_id: Option<RequestId>,
        code: ErrorCode,
        message: String,
    },

    /// Keep-alive pong.
    Pong { seq: u64 },
}
