//! Shared client message dispatch logic used by both the QUIC and TCP transports.
//!
//! Each transport implements [`PaneAttacher`] to handle the transport-specific
//! part of `ClientMessage::Attach` (opening a QUIC uni-stream vs. forwarding
//! over the shared TCP control channel).  Everything else is handled here in
//! [`handle_message`].

use std::collections::HashMap;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, ConnectionId, ErrorCode, ServerMessage,
    SessionEventMsg,
};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, InputLockOutcome, ServerApp};
use crate::auth::validate_token;
use crate::connection::classify_error;

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

// ─── Message dispatch ─────────────────────────────────────────────────────────

/// Dispatch a single [`ClientMessage`] for a connected client.
///
/// Returns `true` to keep reading, `false` to close the connection.
/// The `attacher` is only called for `ClientMessage::Attach`.
pub async fn handle_message<A: PaneAttacher>(
    state: &mut SharedClientState,
    msg: ClientMessage,
    attacher: &A,
) -> bool {
    if !state.authenticated {
        if let ClientMessage::Auth {
            token,
            protocol_version,
            capabilities,
            connection_id: incoming_conn_id,
        } = msg
        {
            if protocol_version != kmux_protocol::messages::PROTOCOL_VERSION {
                state.send(ServerMessage::AuthResult {
                    success: false,
                    reason: Some(format!(
                        "protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    )),
                    client_id: None,
                    server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    connection_id: None,
                });
                warn!(
                    "Protocol version mismatch: client={protocol_version}, server={}",
                    kmux_protocol::messages::PROTOCOL_VERSION
                );
                return false;
            } else if validate_token(&token, &state.app.auth_token) {
                let (client_id, conn_id) = state.app.register_client(incoming_conn_id).await;
                state.client_id = Some(client_id);
                state.connection_id = Some(conn_id);
                state.capabilities = capabilities;
                state.authenticated = true;
                state.send(ServerMessage::AuthResult {
                    success: true,
                    reason: None,
                    client_id: Some(client_id),
                    server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    connection_id: Some(conn_id),
                });
                info!(
                    "{}client {client_id:?} authenticated (conn={conn_id:?})",
                    state.transport_label
                );
            } else {
                state.send(ServerMessage::AuthResult {
                    success: false,
                    reason: Some("invalid token".to_string()),
                    client_id: None,
                    server_version: None,
                    connection_id: None,
                });
                warn!("{}authentication failed", state.transport_label);
            }
        } else {
            state.error(None, ErrorCode::NotAuthenticated, "send Auth first");
        }
        return true;
    }

    let client_id = state.client_id.expect("authenticated without client_id");

    match msg {
        ClientMessage::Auth { .. } => {}

        ClientMessage::ChannelReady => {
            let old = state
                .app
                .complete_channel_switch(state.connection_id.unwrap(), client_id)
                .await;
            if let Some(old_transport) = old {
                state.send(ServerMessage::ChannelSwitched { old_transport });
            }
        }

        ClientMessage::SessionCreate {
            request_id,
            name,
            cwd,
            program,
            args,
            size,
        } => match state
            .app
            .create_session(name, cwd, program, args, size, &state.capabilities)
            .await
        {
            Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::SessionClose {
            request_id,
            word_id,
        } => {
            let pane_ids: Vec<String> = state
                .attached
                .keys()
                .filter(|k| k.starts_with(&format!("{word_id}/")))
                .cloned()
                .collect();
            for pane_id in &pane_ids {
                if let Some(handle) = state.attached.remove(pane_id) {
                    handle.abort();
                }
                state.app.detach_from_pane(pane_id, client_id).await;
            }
            match state.app.close_session(&word_id).await {
                Ok(exit_code) => state.send(ServerMessage::SessionClosed {
                    request_id,
                    word_id,
                    exit_code,
                }),
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::PaneCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => match state
            .app
            .create_pane(&word_id, program, args, size, &state.capabilities)
            .await
        {
            Ok(pane_id) => state.send(ServerMessage::PaneCreated {
                request_id,
                pane_id,
                session_word_id: word_id,
            }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::PaneClose {
            request_id,
            pane_id,
        } => {
            if let Some(handle) = state.attached.remove(&pane_id) {
                handle.abort();
            }
            state.app.detach_from_pane(&pane_id, client_id).await;
            match state.app.close_pane(&pane_id).await {
                Ok(exit_code) => state.send(ServerMessage::PaneClosed {
                    request_id,
                    pane_id,
                    exit_code,
                }),
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::SessionList { request_id } => {
            let sessions = state.app.list_sessions().await;
            state.send(ServerMessage::SessionListResult {
                request_id,
                sessions,
            });
        }

        ClientMessage::PtyInput { pane_id, data } => {
            if let Err(e) = state.app.write_input(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::PtyPaste { pane_id, data } => {
            if let Err(e) = state.app.write_paste(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Resize { pane_id, size } => {
            if let Err(e) = state.app.resize(&pane_id, size).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Attach {
            pane_id,
            last_seqno,
        } => {
            // If already attached, detach first.
            if let Some(old) = state.attached.remove(&pane_id) {
                old.abort();
                state.app.detach_from_pane(&pane_id, client_id).await;
            }

            let (client_tx, client_rx) = mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

            match state
                .app
                .attach(
                    &pane_id,
                    client_id,
                    last_seqno,
                    client_tx,
                    state.ctrl_tx.clone(),
                    state.capabilities.clone(),
                )
                .await
            {
                Ok(result) => {
                    match attacher
                        .start_pane_stream(pane_id.clone(), result, client_rx)
                        .await
                    {
                        Ok(handle) => {
                            state.attached.insert(pane_id, handle);
                        }
                        Err(e) => {
                            state.error(None, ErrorCode::InternalError, e);
                        }
                    }
                }
                Err(e) => state.error(None, classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::Detach { pane_id } => {
            if let Some(handle) = state.attached.remove(&pane_id) {
                handle.abort();
                state.app.detach_from_pane(&pane_id, client_id).await;
                debug!("{}detached from pane '{pane_id}'", state.transport_label);
            }
        }

        ClientMessage::Signal { pane_id, signal } => {
            if let Err(e) = state.app.send_signal(&pane_id, signal).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::RequestInputLock { pane_id } => {
            match state.app.request_input_lock(&pane_id, client_id).await {
                Ok(InputLockOutcome::Granted) => {
                    state.send(ServerMessage::InputLockGranted { pane_id });
                }
                Ok(InputLockOutcome::Denied(holder)) => {
                    state.send(ServerMessage::InputLockDenied { pane_id, holder });
                }
                Err(e) => state.error(None, classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::ReleaseInputLock { pane_id } => {
            match state.app.release_input_lock(&pane_id, client_id).await {
                Ok(true) => state.send(ServerMessage::InputLockReleased { pane_id }),
                Ok(false) => {}
                Err(e) => state.error(None, classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::SessionRename {
            request_id,
            word_id,
            new_name,
        } => match state.app.rename_session(&word_id, &new_name).await {
            Ok(()) => state.send(ServerMessage::SessionRenamed { word_id, new_name }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::SetSnapshotMode { enabled } => {
            state.app.set_snapshot_mode(client_id, enabled).await;
            debug!(
                "{}client {client_id:?} snapshot mode = {enabled}",
                state.transport_label
            );
        }

        ClientMessage::Ping { seq } => {
            state.send(ServerMessage::Pong { seq });
        }

        ClientMessage::Pong { .. } => {}
    }

    true
}

// ─── Shared helpers ───────────────────────────────────────────────────────────

/// Translate a kmux-pty lifecycle event into a protocol [`SessionEventMsg`].
///
/// The pty registry uses `pane_id` as the session name.
pub fn pty_event_to_msg(event: kmux_pty::events::SessionEvent) -> SessionEventMsg {
    match event {
        kmux_pty::events::SessionEvent::Spawned { name } => {
            SessionEventMsg::PaneSpawned { pane_id: name }
        }
        kmux_pty::events::SessionEvent::Exited { name, status } => SessionEventMsg::PaneExited {
            pane_id: name,
            code: status.code(),
            signal: match status {
                kmux_pty::process::ExitStatus::Signal(s) => Some(s),
                _ => None,
            },
        },
        kmux_pty::events::SessionEvent::Resized { name, rows, cols } => {
            SessionEventMsg::PaneResized {
                pane_id: name,
                rows,
                cols,
            }
        }
        kmux_pty::events::SessionEvent::Closed { name } => {
            SessionEventMsg::PaneClosed { pane_id: name }
        }
        kmux_pty::events::SessionEvent::Timeout { name, .. } => {
            SessionEventMsg::PaneClosed { pane_id: name }
        }
    }
}
