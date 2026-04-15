use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, ConnectionId, ErrorCode, ServerMessage,
    SessionEventMsg, epoch_millis,
};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use quinn::Connection;
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
    /// Connection identity assigned on first auth; used for channel switching.
    connection_id: Option<ConnectionId>,
    /// Rendering capabilities declared by this client at Auth time.
    capabilities: ClientCapabilities,
    /// Output-forwarding task handles, keyed by pane_id.
    attached: HashMap<String, AbortHandle>,
    /// Sender for the control stream writer task.
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    /// QUIC connection handle -- used to open uni streams for pane diffs.
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
            connection_id: None,
            capabilities: ClientCapabilities::default(),
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

    /// Handle a single client message. Returns `true` to keep reading, `false`
    /// to signal the caller to close the connection (e.g. after version mismatch).
    async fn handle(&mut self, msg: ClientMessage) -> bool {
        if !self.authenticated {
            if let ClientMessage::Auth {
                token,
                protocol_version,
                capabilities,
                connection_id: incoming_conn_id,
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
                        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                        connection_id: None,
                    });
                    warn!(
                        "Protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    );
                    return false;
                } else if validate_token(&token, &self.app.auth_token) {
                    let (client_id, conn_id) = self.app.register_client(incoming_conn_id).await;
                    self.client_id = Some(client_id);
                    self.connection_id = Some(conn_id);
                    self.capabilities = capabilities;
                    self.authenticated = true;
                    self.send(ServerMessage::AuthResult {
                        success: true,
                        reason: None,
                        client_id: Some(client_id),
                        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                        connection_id: Some(conn_id),
                    });
                    info!("Client {client_id:?} authenticated (conn={conn_id:?})");
                } else {
                    self.send(ServerMessage::AuthResult {
                        success: false,
                        reason: Some("invalid token".to_string()),
                        client_id: None,
                        server_version: None,
                        connection_id: None,
                    });
                    warn!("Authentication failed");
                }
            } else {
                self.error(None, ErrorCode::NotAuthenticated, "send Auth first");
            }
            return true;
        }

        let client_id = self.client_id.expect("authenticated without client_id");

        match msg {
            ClientMessage::Auth { .. } => {}

            ClientMessage::ChannelReady => {
                // The client has successfully established this channel and is
                // signalling it is ready to use it as the primary transport.
                // Confirm the switch so the client can close the old channel.
                let old = self
                    .app
                    .complete_channel_switch(self.connection_id.unwrap(), client_id)
                    .await;
                if let Some(old_transport) = old {
                    self.send(ServerMessage::ChannelSwitched { old_transport });
                }
            }

            ClientMessage::SessionCreate {
                request_id,
                name,
                cwd,
                program,
                args,
                size,
            } => match self
                .app
                .create_session(name, cwd, program, args, size, &self.capabilities)
                .await
            {
                Ok(entry) => self.send(ServerMessage::SessionCreated { request_id, entry }),
                Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
            },

            ClientMessage::SessionClose {
                request_id,
                word_id,
            } => {
                // Detach from all panes in this session
                let pane_ids: Vec<String> = self
                    .attached
                    .keys()
                    .filter(|k| k.starts_with(&format!("{word_id}/")))
                    .cloned()
                    .collect();
                for pane_id in &pane_ids {
                    if let Some(handle) = self.attached.remove(pane_id) {
                        handle.abort();
                    }
                    self.app.detach_from_pane(pane_id, client_id).await;
                }
                match self.app.close_session(&word_id).await {
                    Ok(exit_code) => self.send(ServerMessage::SessionClosed {
                        request_id,
                        word_id,
                        exit_code,
                    }),
                    Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::PaneCreate {
                request_id,
                word_id,
                program,
                args,
                size,
            } => match self
                .app
                .create_pane(&word_id, program, args, size, &self.capabilities)
                .await
            {
                Ok(pane_id) => self.send(ServerMessage::PaneCreated {
                    request_id,
                    pane_id,
                    session_word_id: word_id,
                }),
                Err(e) => self.error(Some(request_id), classify_error(&e), e.to_string()),
            },

            ClientMessage::PaneClose {
                request_id,
                pane_id,
            } => {
                if let Some(handle) = self.attached.remove(&pane_id) {
                    handle.abort();
                }
                self.app.detach_from_pane(&pane_id, client_id).await;
                match self.app.close_pane(&pane_id).await {
                    Ok(exit_code) => self.send(ServerMessage::PaneClosed {
                        request_id,
                        pane_id,
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

            ClientMessage::PtyInput { pane_id, data } => {
                if let Err(e) = self.app.write_input(&pane_id, client_id, data).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::PtyPaste { pane_id, data } => {
                if let Err(e) = self.app.write_paste(&pane_id, client_id, data).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::Resize { pane_id, size } => {
                if let Err(e) = self.app.resize(&pane_id, size).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::Attach {
                pane_id,
                last_seqno,
            } => {
                // If already attached, detach first.
                if let Some(old) = self.attached.remove(&pane_id) {
                    old.abort();
                    self.app.detach_from_pane(&pane_id, client_id).await;
                }

                let (client_tx, mut client_rx) =
                    mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

                match self
                    .app
                    .attach(
                        &pane_id,
                        client_id,
                        last_seqno,
                        client_tx,
                        self.ctrl_tx.clone(),
                        self.capabilities.clone(),
                    )
                    .await
                {
                    Ok(result) => {
                        let uni_stream = match self.conn.open_uni().await {
                            Ok(s) => s,
                            Err(e) => {
                                self.error(
                                    None,
                                    ErrorCode::InternalError,
                                    format!("failed to open uni stream: {e}"),
                                );
                                return true;
                            }
                        };

                        let pane_id_clone = pane_id.clone();
                        let handle = tokio::spawn(async move {
                            pane_uni_writer(uni_stream, result, pane_id_clone, &mut client_rx)
                                .await;
                        })
                        .abort_handle();
                        self.attached.insert(pane_id, handle);
                    }
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::Detach { pane_id } => {
                if let Some(handle) = self.attached.remove(&pane_id) {
                    handle.abort();
                    self.app.detach_from_pane(&pane_id, client_id).await;
                    debug!("Detached from pane '{pane_id}'");
                }
            }

            ClientMessage::Signal { pane_id, signal } => {
                if let Err(e) = self.app.send_signal(&pane_id, signal).await {
                    self.error(None, classify_error(&e), e.to_string());
                }
            }

            ClientMessage::RequestInputLock { pane_id } => {
                match self.app.request_input_lock(&pane_id, client_id).await {
                    Ok(InputLockOutcome::Granted) => {
                        self.send(ServerMessage::InputLockGranted { pane_id });
                    }
                    Ok(InputLockOutcome::Denied(holder)) => {
                        self.send(ServerMessage::InputLockDenied { pane_id, holder });
                    }
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::ReleaseInputLock { pane_id } => {
                match self.app.release_input_lock(&pane_id, client_id).await {
                    Ok(true) => self.send(ServerMessage::InputLockReleased { pane_id }),
                    Ok(false) => {}
                    Err(e) => self.error(None, classify_error(&e), e.to_string()),
                }
            }

            ClientMessage::SessionRename {
                request_id,
                word_id,
                new_name,
            } => match self.app.rename_session(&word_id, &new_name).await {
                Ok(()) => self.send(ServerMessage::SessionRenamed { word_id, new_name }),
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

        true
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
async fn pane_uni_writer(
    mut uni: quinn::SendStream,
    attach_result: AttachResult,
    pane_id: String,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
) {
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => {
            let msg = ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
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
                    pane_id: pane_id.clone(),
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
                pane_id: pane_id.clone(),
            };
            if send_frame(&mut uni, &reset_msg).await.is_err() {
                return;
            }
            let msg = ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            };
            if send_frame(&mut uni, &msg).await.is_err() {
                return;
            }
        }
    }

    while let Some(msg) = client_rx.recv().await {
        let write_start = Instant::now();
        if send_frame(&mut uni, &msg).await.is_err() {
            break;
        }
        let write_us = write_start.elapsed().as_micros();
        if write_us > 1000 {
            debug!(pane_id, write_us, "slow uni stream write");
        }
    }

    let _ = uni.finish();
}

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

pub fn classify_error(e: &kmux_pty::error::KmuxError) -> ErrorCode {
    match e {
        kmux_pty::error::KmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        kmux_pty::error::KmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
        kmux_pty::error::KmuxError::Pty(err) if *err == nix::Error::EPERM => ErrorCode::InputLocked,
        _ => ErrorCode::InternalError,
    }
}

/// Handle a single QUIC client connection.
pub async fn handle(conn: Connection, app: Arc<ServerApp>) {
    let (ctrl_send, mut ctrl_recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => {
            warn!("Failed to accept control stream: {e}");
            return;
        }
    };

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

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

    let mut event_rx = app.subscribe_events();
    let event_tx = ctrl_tx.clone();
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let msg = pty_event_to_msg(event);
            let _ = event_tx.send(ServerMessage::Event { event: msg });
        }
    });

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

    loop {
        match read_frame(&mut ctrl_recv).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => {
                    if !state.handle(client_msg).await {
                        break;
                    }
                }
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
    if let Some(conn_id) = state.connection_id {
        app.unregister_client(conn_id).await;
    }

    drop(state);
    writer_task.abort();
    info!("Connection closed");
}

/// Translate a kmux-pty lifecycle event into a protocol `SessionEventMsg`.
/// The pty registry uses `pane_id` as the session name.
fn pty_event_to_msg(event: kmux_pty::events::SessionEvent) -> SessionEventMsg {
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
