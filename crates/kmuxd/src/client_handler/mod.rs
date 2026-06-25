mod dispatch;
mod events;
mod session;
pub use dispatch::handle_message;
pub use events::pty_event_to_msg;
pub(crate) use session::MAX_WRITE_BATCH;
pub use session::{build_attach_replay, run_client_session};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kmux_protocol::Compressor;
use kmux_protocol::TransportKind;
use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ConnectionId, ErrorCode, ServerMessage,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::Span;

use crate::app::{AttachResult, ConnectionMetrics, ServerApp};

/// Per-client output channel capacity (number of `ServerMessage` items buffered).
pub const CLIENT_CHANNEL_CAPACITY: usize = 512;

// ─── Outbound compression state ────────────────────────────────────────────────

/// Per-connection outbound compression, shared between the auth handler (which
/// flips `enabled` once the daemon decides, in [`handle_message`]) and the
/// writer / pane-attacher tasks (which read it per frame). `level` and
/// `min_size` are connection constants taken from `[compression]` config.
pub struct OutboundCompression {
    enabled: AtomicBool,
    level: i32,
    min_size: usize,
}

impl OutboundCompression {
    pub fn new(level: i32, min_size: usize) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            level,
            min_size,
        }
    }

    /// Enable or disable compression for subsequent outbound frames.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// The [`Compressor`] to apply to the next outbound frame.
    pub fn compressor(&self) -> Compressor {
        if self.enabled.load(Ordering::Relaxed) {
            Compressor::Zstd {
                level: self.level,
                min_size: self.min_size,
            }
        } else {
            Compressor::Off
        }
    }
}

// ─── PaneAttacher trait ───────────────────────────────────────────────────────

/// Abstracts the transport-specific part of a pane `Attach`: given the
/// `AttachResult` from the app layer and a receiver of live `ServerMessage`
/// frames, start a background task that streams pane diffs to the client and
/// return its [`AbortHandle`].
pub trait PaneAttacher: Send + Sync {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl std::future::Future<Output = Result<AbortHandle, String>> + Send;
}

// ─── Shared client state ──────────────────────────────────────────────────────

/// State held between the `Auth` and `AuthProof` handshake messages (issue #146):
/// the random nonce the daemon issued plus the identity claim it will trust once
/// the client returns a valid signature over that nonce.
pub struct PendingAuth {
    pub nonce: Vec<u8>,
    pub public_key: Vec<u8>,
    pub hostname: String,
    pub username: String,
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
}

/// Transport-independent state for a connected client.
pub struct SharedClientState {
    pub authenticated: bool,
    pub client_id: Option<ClientId>,
    pub connection_id: Option<ConnectionId>,
    pub capabilities: ClientCapabilities,
    /// Set after a valid `Auth`; consumed when `AuthProof` arrives (issue #146).
    pub pending_auth: Option<PendingAuth>,
    /// This connection's verified identity fingerprint, once authenticated.
    pub machine_id: Option<String>,
    /// This connection's daemon-assigned user-readable label, once authenticated.
    pub label: Option<String>,
    /// Output-forwarding task handles, keyed by pane_id.
    pub attached: HashMap<String, AbortHandle>,
    /// Sender for the control-stream writer task.
    pub ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    pub app: Arc<ServerApp>,
    /// Connection-scoped tracing span; conn_id and client_id are recorded into
    /// it once authentication completes so every subsequent log line carries them.
    pub conn_span: Span,
    /// Transport kind that accepted this connection.
    pub transport: TransportKind,
    /// Live per-connection byte/activity counters shared with the I/O tasks.
    pub metrics: Arc<ConnectionMetrics>,
    /// Outbound compression toggle, shared with the writer + pane-attacher tasks.
    /// Flipped by the auth handler once the daemon decides (see [`handle_message`]).
    pub comp_out: Arc<OutboundCompression>,
    /// When this connection resumes an existing `conn_id` (channel switch in
    /// progress), holds the *previous* transport that was attached to that
    /// `conn_id`. Consumed when `ChannelReady` arrives so the server can send
    /// `ChannelSwitched { old_transport }` with the genuinely-old name. `None`
    /// for fresh connections that aren't a channel switch.
    pub pending_swap_from: Option<TransportKind>,
}

impl SharedClientState {
    pub fn new(
        app: Arc<ServerApp>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        conn_span: Span,
        transport: TransportKind,
        metrics: Arc<ConnectionMetrics>,
        comp_out: Arc<OutboundCompression>,
    ) -> Self {
        Self {
            authenticated: false,
            client_id: None,
            connection_id: None,
            capabilities: ClientCapabilities::default(),
            pending_auth: None,
            machine_id: None,
            label: None,
            attached: HashMap::new(),
            ctrl_tx,
            app,
            conn_span,
            transport,
            metrics,
            comp_out,
            pending_swap_from: None,
        }
    }

    pub fn send(&self, msg: ServerMessage) {
        let _ = self.ctrl_tx.send(msg);
    }

    pub fn error(&self, req: Option<u64>, code: ErrorCode, message: impl Into<String>) {
        self.send(ServerMessage::Error {
            request_id: req,
            code,
            message: message.into(),
        });
    }
}

impl Drop for SharedClientState {
    fn drop(&mut self) {
        for (_, handle) in self.attached.drain() {
            handle.abort();
        }
    }
}
