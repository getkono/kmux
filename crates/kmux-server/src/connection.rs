use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::Connection;
use kmux_protocol::messages::{
    ClientId, ClientMessage, ErrorCode, ServerMessage, SessionEventMsg, epoch_millis,
};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, InputLockOutcome, ServerApp};
use crate::auth::validate_token;

/// Per-client output channel capacity (number of `ServerMessage` items buffered).
const CLIENT_CHANNEL_CAPACITY: usize = 512;

/// State for a single connected client.
struct ClientState {
    authenticated: bool,
    client_id: Option<ClientId>,
    /// Output-forwarding task handles, keyed by session name.
    attached: HashMap<String, AbortHandle>,
    /// Sender for the control stream writer task.
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    /// QUIC connection handle -- used to open uni streams for session diffs.
    conn: Connection,
    app: Arc<ServerApp>,
}

impl ClientState {
    fn new(
        app: Arc<ServerApp>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
        conn: Connection,
    ) -> Self {
        Self {
            authenticated: false,
            client_id: None,
            attached: HashMap::new(),
            ctrl_tx,
            conn,
            app,
        }
    }

    fn send(&self, msg: ServerMessage) {
        let _ = self.ctrl_tx.send(msg);
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
            if let ClientMessage::Auth {
                token,
                protocol_version,
            } = msg
            {
                if protocol_version != kmux_protocol::messages::PROTOCOL_VERSION {
                    self.send(ServerMessage::AuthResult {
                        success: false,
                        reason: Some(format!(
                            "protocol version mismatch: client={protocol_version}, server={}",
                            kmux_protocol::messages::PROTOCOL_VERSION
                        )),
                        client_id: None,
                    });
                    warn!(
                        "Protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    );
                } else if validate_token(&token, &self.app.auth_token) {
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

        let client_id = self.client_id.expect("authenticated without client_id");

        match msg {
            ClientMessage::Auth { .. } => {}

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

            ClientMessage::PtyPaste { session, data } => {
                if let Err(e) = self.app.write_paste(&session, client_id, data).await {
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
                    .attach(
                        &session,
                        client_id,
                        last_seqno,
                        client_tx,
                        self.ctrl_tx.clone(),
                    )
                    .await
                {
                    Ok(result) => {
                        // Open a server-initiated unidirectional stream for this session's diffs.
                        let uni_stream = match self.conn.open_uni().await {
                            Ok(s) => s,
                            Err(e) => {
                                self.error(
                                    None,
                                    ErrorCode::InternalError,
                                    format!("failed to open uni stream: {e}"),
                                );
                                return;
                            }
                        };

                        let session_name = session.clone();
                        let handle = tokio::spawn(async move {
                            session_uni_writer(uni_stream, result, session_name, &mut client_rx)
                                .await;
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
                    Ok(false) => {}
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

            ClientMessage::SetSnapshotMode { enabled } => {
                self.app.set_snapshot_mode(client_id, enabled).await;
                debug!("Client {client_id:?} snapshot mode = {enabled}");
            }

            ClientMessage::Ping { seq } => {
                self.send(ServerMessage::Pong { seq });
            }

            ClientMessage::Pong { .. } => {}
        }
    }
}

impl Drop for ClientState {
    fn drop(&mut self) {
        for (_, handle) in self.attached.drain() {
            handle.abort();
        }
    }
}

/// Write initial replay data + live diffs on a server-initiated unidirectional stream.
async fn session_uni_writer(
    mut uni: quinn::SendStream,
    attach_result: AttachResult,
    session: String,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
) {
    // Send initial replay data
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => {
            let msg = ServerMessage::TerminalSnapshot {
                session: session.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            };
            if send_frame(&mut uni, &msg).await.is_err() {
                return;
            }
        }
        AttachResult::Delta(diffs) => {
            for (seqno, diff) in diffs {
                let msg = ServerMessage::TerminalUpdate {
                    session: session.clone(),
                    diff,
                    seqno,
                    sent_at_ms: epoch_millis(),
                };
                if send_frame(&mut uni, &msg).await.is_err() {
                    return;
                }
            }
        }
        AttachResult::SyncReset(snapshot, seqno) => {
            let reset_msg = ServerMessage::SyncReset {
                session: session.clone(),
            };
            if send_frame(&mut uni, &reset_msg).await.is_err() {
                return;
            }
            let msg = ServerMessage::TerminalSnapshot {
                session: session.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            };
            if send_frame(&mut uni, &msg).await.is_err() {
                return;
            }
        }
    }

    // Forward live diffs from the relay task
    while let Some(msg) = client_rx.recv().await {
        let write_start = Instant::now();
        if send_frame(&mut uni, &msg).await.is_err() {
            break;
        }
        let write_us = write_start.elapsed().as_micros();
        if write_us > 1000 {
            debug!(session, write_us, "slow uni stream write");
        }
    }

    let _ = uni.finish();
}

/// Encode a `ServerMessage` and write it as a length-prefixed frame.
async fn send_frame(
    stream: &mut quinn::SendStream,
    msg: &ServerMessage,
) -> Result<(), kmux_protocol::ProtocolError> {
    let bytes = encode_server(msg)?;
    if bytes.len() > 4096 {
        debug!(frame_bytes = bytes.len(), "large frame");
    }
    write_frame(stream, &bytes).await
}

/// Map kmux errors to protocol error codes.
fn classify_error(e: &kmux_pty::error::kmuxError) -> ErrorCode {
    match e {
        kmux_pty::error::kmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        kmux_pty::error::kmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
        kmux_pty::error::kmuxError::Pty(err) if *err == nix::Error::EPERM => ErrorCode::InputLocked,
        _ => ErrorCode::InternalError,
    }
}

/// Handle a single QUIC client connection.
///
/// Uses a multi-stream model:
/// - Bidirectional stream 0 (control): client<->server control messages
/// - Unidirectional streams (per-session): server->client terminal diffs
pub async fn handle(conn: Connection, app: Arc<ServerApp>) {
    // Accept the first bidirectional stream as the control channel
    let (ctrl_send, mut ctrl_recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            warn!("Failed to accept control stream: {e}");
            return;
        }
    };

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Control stream writer task
    let writer_task = tokio::spawn(async move {
        let mut ctrl_send = ctrl_send;
        while let Some(msg) = ctrl_rx.recv().await {
            match encode_server(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut ctrl_send, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("Failed to encode server message: {e}"),
            }
        }
        let _ = ctrl_send.finish();
    });

    // Forward lifecycle events to this client on the control stream
    let mut event_rx = app.subscribe_events();
    let event_tx = ctrl_tx.clone();
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let msg = session_event_to_msg(event);
            let _ = event_tx.send(ServerMessage::Event { event: msg });
        }
    });

    // Periodic application-level keepalive ping on the control stream
    let ping_tx = ctrl_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut seq = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if ping_tx.send(ServerMessage::Ping { seq }).is_err() {
                break;
            }
            seq += 1;
        }
    });

    let mut state = ClientState::new(app.clone(), ctrl_tx, conn);

    // Main read loop on control stream
    loop {
        match read_frame(&mut ctrl_recv).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => state.handle(client_msg).await,
                Err(e) => {
                    warn!("Failed to decode client message: {e}");
                    state.error(None, ErrorCode::InvalidMessage, e.to_string());
                }
            },
            Ok(None) => {
                debug!("Control stream closed");
                break;
            }
            Err(e) => {
                warn!("Control stream read error: {e}");
                break;
            }
        }
    }

    event_task.abort();
    ping_task.abort();

    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }

    drop(state);
    writer_task.abort();
    info!("Connection closed");
}

fn session_event_to_msg(event: kmux_pty::events::SessionEvent) -> SessionEventMsg {
    match event {
        kmux_pty::events::SessionEvent::Spawned { name } => SessionEventMsg::Spawned { name },
        kmux_pty::events::SessionEvent::Exited { name, status } => SessionEventMsg::Exited {
            name,
            code: status.code(),
            signal: match status {
                kmux_pty::process::ExitStatus::Signal(s) => Some(s),
                _ => None,
            },
        },
        kmux_pty::events::SessionEvent::Resized { name, rows, cols } => {
            SessionEventMsg::Resized { name, rows, cols }
        }
        kmux_pty::events::SessionEvent::Closed { name } => SessionEventMsg::Closed { name },
        kmux_pty::events::SessionEvent::Timeout { name, .. } => SessionEventMsg::Closed { name },
    }
}
