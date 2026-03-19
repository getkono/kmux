use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use smux_protocol::messages::{
    ClientId, ClientMessage, ErrorCode, SequenceNo, ServerMessage, SessionEventMsg,
};
use smux_protocol::{decode_client, encode_server};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, InputLockOutcome, ServerApp};
use crate::auth::validate_token;

type WsStream = WebSocketStream<tokio_rustls::server::TlsStream<TcpStream>>;

/// Per-client output channel capacity (number of `ServerMessage` items buffered).
const CLIENT_CHANNEL_CAPACITY: usize = 512;

/// State for a single connected client.
struct ClientState {
    authenticated: bool,
    client_id: Option<ClientId>,
    /// Output-forwarding task handles, keyed by session name.
    attached: HashMap<String, AbortHandle>,
    writer_tx: mpsc::UnboundedSender<ServerMessage>,
    app: Arc<ServerApp>,
}

impl ClientState {
    fn new(app: Arc<ServerApp>, writer_tx: mpsc::UnboundedSender<ServerMessage>) -> Self {
        Self {
            authenticated: false,
            client_id: None,
            attached: HashMap::new(),
            writer_tx,
            app,
        }
    }

    fn send(&self, msg: ServerMessage) {
        let _ = self.writer_tx.send(msg);
    }

    fn error(&self, req: Option<u64>, code: ErrorCode, message: impl Into<String>) {
        self.send(ServerMessage::Error {
            request_id: req,
            code,
            message: message.into(),
        });
    }

    async fn handle(&mut self, msg: ClientMessage) {
        if !self.authenticated {
            // Only Auth is allowed before authentication
            if let ClientMessage::Auth { token, .. } = msg {
                if validate_token(&token, &self.app.auth_token) {
                    let id = self.app.next_client_id();
                    self.client_id = Some(id);
                    self.authenticated = true;
                    self.send(ServerMessage::AuthResult {
                        success: true,
                        reason: None,
                        client_id: Some(id),
                    });
                    info!("Client {id:?} authenticated");
                } else {
                    self.send(ServerMessage::AuthResult {
                        success: false,
                        reason: Some("invalid token".to_string()),
                        client_id: None,
                    });
                    warn!("Authentication failed");
                }
            } else {
                self.error(None, ErrorCode::NotAuthenticated, "send Auth first");
            }
            return;
        }

        // Authenticated clients always have a client_id assigned.
        let client_id = self.client_id.expect("authenticated without client_id");

        match msg {
            ClientMessage::Auth { .. } => {
                // Already authenticated — ignore
            }

            ClientMessage::SessionCreate {
                request_id,
                name,
                program,
                args,
                size,
            } => match self.app.create_session(&name, program, args, size).await {
                Ok(()) => self.send(ServerMessage::SessionCreated { request_id, name }),
                Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
            },

            ClientMessage::SessionClose { request_id, name } => {
                // Detach first if attached
                if let Some(handle) = self.attached.remove(&name) {
                    handle.abort();
                }
                self.app.detach_from_session(&name, client_id).await;
                match self.app.close_session(&name).await {
                    Ok(exit_code) => self.send(ServerMessage::SessionClosed {
                        request_id,
                        name,
                        exit_code,
                    }),
                    Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::SessionList { request_id } => {
                let sessions = self.app.list_sessions().await;
                self.send(ServerMessage::SessionListResult {
                    request_id,
                    sessions,
                });
            }

            ClientMessage::PtyInput { session, data } => {
                if let Err(e) = self.app.write_input(&session, client_id, data).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::Resize { session, size } => {
                if let Err(e) = self.app.resize(&session, size).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::Attach {
                session,
                last_seqno,
            } => {
                // If already attached, detach first.
                if let Some(old) = self.attached.remove(&session) {
                    old.abort();
                    self.app.detach_from_session(&session, client_id).await;
                }

                let (client_tx, mut client_rx) =
                    mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

                match self
                    .app
                    .attach(&session, client_id, last_seqno, client_tx)
                    .await
                {
                    Ok(result) => {
                        // Replay scrollback before the live stream begins.
                        const CHUNK: usize = 64 * 1024;
                        match result {
                            AttachResult::FullSnapshot(bytes) => {
                                for chunk in bytes.chunks(CHUNK) {
                                    let _ = self.writer_tx.send(ServerMessage::PtyOutput {
                                        session: session.clone(),
                                        data: chunk.to_vec(),
                                        seqno: SequenceNo(0),
                                    });
                                }
                            }
                            AttachResult::Delta(chunks) => {
                                for (seqno, data) in chunks {
                                    let _ = self.writer_tx.send(ServerMessage::PtyOutput {
                                        session: session.clone(),
                                        data,
                                        seqno,
                                    });
                                }
                            }
                            AttachResult::SyncReset(bytes) => {
                                let _ = self.writer_tx.send(ServerMessage::SyncReset {
                                    session: session.clone(),
                                });
                                for chunk in bytes.chunks(CHUNK) {
                                    let _ = self.writer_tx.send(ServerMessage::PtyOutput {
                                        session: session.clone(),
                                        data: chunk.to_vec(),
                                        seqno: SequenceNo(0),
                                    });
                                }
                            }
                        }

                        // Forward live output from the per-client channel to the writer.
                        let tx = self.writer_tx.clone();
                        let handle = tokio::spawn(async move {
                            while let Some(msg) = client_rx.recv().await {
                                if tx.send(msg).is_err() {
                                    break;
                                }
                            }
                        })
                        .abort_handle();
                        self.attached.insert(session, handle);
                    }
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::Detach { session } => {
                if let Some(handle) = self.attached.remove(&session) {
                    handle.abort();
                    self.app.detach_from_session(&session, client_id).await;
                    debug!("Detached from session '{session}'");
                }
            }

            ClientMessage::Signal { session, signal } => {
                if let Err(e) = self.app.send_signal(&session, signal).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::RequestInputLock { session } => {
                match self.app.request_input_lock(&session, client_id).await {
                    Ok(InputLockOutcome::Granted) => {
                        self.send(ServerMessage::InputLockGranted { session });
                    }
                    Ok(InputLockOutcome::Denied(holder)) => {
                        self.send(ServerMessage::InputLockDenied { session, holder });
                    }
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::ReleaseInputLock { session } => {
                match self.app.release_input_lock(&session, client_id).await {
                    Ok(true) => self.send(ServerMessage::InputLockReleased { session }),
                    Ok(false) => {} // Lock not held by this client; silently ignore
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::SessionRename {
                request_id,
                session,
                new_name,
            } => match self.app.rename_session(&session, &new_name).await {
                Ok(()) => self.send(ServerMessage::SessionRenamed {
                    old_name: session,
                    new_name,
                }),
                Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
            },

            ClientMessage::Ping { seq } => {
                self.send(ServerMessage::Pong { seq });
            }

            ClientMessage::Pong { .. } => {
                // Server-initiated ping round-trip complete; nothing to do.
            }
        }
    }
}

impl Drop for ClientState {
    fn drop(&mut self) {
        // Abort all output-forwarding tasks when the connection closes.
        // ClientMap cleanup is handled by `detach_client_all` called in `handle`.
        for (_, handle) in self.attached.drain() {
            handle.abort();
        }
    }
}

/// Map smux errors to protocol error codes.
fn classify_error(e: &smux::error::SmuxError) -> ErrorCode {
    match e {
        smux::error::SmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        smux::error::SmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
        smux::error::SmuxError::Pty(err) if *err == nix::Error::EPERM => ErrorCode::InputLocked,
        _ => ErrorCode::InternalError,
    }
}

/// Handle a single WebSocket client connection.
pub async fn handle(ws: WsStream, app: Arc<ServerApp>) {
    let (ws_sink, mut ws_stream) = ws.split();
    let (writer_tx, writer_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Spawn a writer task that serialises ServerMessages onto the WebSocket
    let writer_task = tokio::spawn(writer_loop(writer_rx, ws_sink));

    // Forward lifecycle events from the global event bus to this client
    let mut event_rx = app.subscribe_events();
    let event_tx = writer_tx.clone();
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let msg = session_event_to_msg(event);
            let _ = event_tx.send(ServerMessage::Event { event: msg });
        }
    });

    let mut state = ClientState::new(app.clone(), writer_tx);

    // Main read loop
    while let Some(frame) = ws_stream.next().await {
        match frame {
            Ok(Message::Binary(data)) => match decode_client(&data) {
                Ok(client_msg) => state.handle(client_msg).await,
                Err(e) => {
                    warn!("Failed to decode client message: {e}");
                    state.error(None, ErrorCode::InvalidMessage, e.to_string());
                }
            },
            Ok(Message::Close(_)) => {
                debug!("Client sent Close frame");
                break;
            }
            Ok(Message::Ping(payload)) => {
                // tungstenite auto-replies with Pong, but we handle it explicitly here
                debug!("Received WebSocket Ping");
                let _ = state.writer_tx.send(ServerMessage::Pong {
                    seq: u64::from_le_bytes(
                        payload
                            .get(..8)
                            .and_then(|b| b.try_into().ok())
                            .unwrap_or([0; 8]),
                    ),
                });
            }
            Ok(_) => {} // Text, Pong — ignore
            Err(e) => {
                warn!("WebSocket error: {e}");
                break;
            }
        }
    }

    event_task.abort();

    // Remove this client from all session relay maps before dropping state.
    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }

    drop(state); // aborts all attached output tasks via Drop impl
    writer_task.abort();
    info!("Connection closed");
}

/// Drain the writer channel and send each message as a WebSocket binary frame.
async fn writer_loop(
    mut rx: mpsc::UnboundedReceiver<ServerMessage>,
    mut sink: futures_util::stream::SplitSink<WsStream, Message>,
) {
    while let Some(msg) = rx.recv().await {
        match encode_server(&msg) {
            Ok(bytes) => {
                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
            Err(e) => warn!("Failed to encode server message: {e}"),
        }
    }
    let _ = sink.close().await;
}

fn session_event_to_msg(event: smux::events::SessionEvent) -> SessionEventMsg {
    match event {
        smux::events::SessionEvent::Spawned { name } => SessionEventMsg::Spawned { name },
        smux::events::SessionEvent::Exited { name, status } => SessionEventMsg::Exited {
            name,
            code: status.code(),
            signal: match status {
                smux::process::ExitStatus::Signal(s) => Some(s),
                _ => None,
            },
        },
        smux::events::SessionEvent::Resized { name, rows, cols } => {
            SessionEventMsg::Resized { name, rows, cols }
        }
        smux::events::SessionEvent::Closed { name } => SessionEventMsg::Closed { name },
        smux::events::SessionEvent::Timeout { name, .. } => SessionEventMsg::Closed { name },
    }
}
