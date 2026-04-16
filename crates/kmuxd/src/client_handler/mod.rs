mod dispatch;
mod events;
mod session;
pub use dispatch::handle_message;
pub use events::pty_event_to_msg;
pub use session::{build_attach_replay, run_client_session};

use std::collections::HashMap;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ConnectionId, ErrorCode, ServerMessage,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::app::{AttachResult, ServerApp};

/// Per-client output channel capacity (number of `ServerMessage` items buffered).
pub const CLIENT_CHANNEL_CAPACITY: usize = 512;

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

/// Transport-independent state for a connected client.
pub struct SharedClientState {
    pub authenticated: bool,
    pub client_id: Option<ClientId>,
    pub connection_id: Option<ConnectionId>,
    pub capabilities: ClientCapabilities,
    /// Output-forwarding task handles, keyed by pane_id.
    pub attached: HashMap<String, AbortHandle>,
    /// Sender for the control-stream writer task.
    pub ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    pub app: std::sync::Arc<ServerApp>,
    /// Short label used in log messages, e.g. `""` (QUIC) or `"TCP "`.
    pub transport_label: &'static str,
}

impl SharedClientState {
    pub fn new(
        app: std::sync::Arc<ServerApp>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        transport_label: &'static str,
    ) -> Self {
        Self {
            authenticated: false,
            client_id: None,
            connection_id: None,
            capabilities: ClientCapabilities::default(),
            attached: HashMap::new(),
            ctrl_tx,
            app,
            transport_label,
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
