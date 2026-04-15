use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, ConnectionId, ErrorCode, ServerMessage,
    SessionEventMsg, epoch_millis,
};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, InputLockOutcome, ServerApp};
use crate::auth::validate_token;
use crate::connection::classify_error;

/// Per-client output channel capacity (number of `ServerMessage` items buffered).
const CLIENT_CHANNEL_CAPACITY: usize = 512;

/// Bind a TCP listener and accept connections in a loop.
///
/// Each accepted connection is handled in its own task using the same
/// `ClientMessage` / `ServerMessage` protocol as the QUIC transport, but over a
/// single TCP byte stream with length-prefixed postcard frames.
pub async fn serve_tcp(addr: SocketAddr, app: Arc<ServerApp>) -> anyhow::Result<u16> {
    let listener = TcpListener::bind(addr).await?;
    let actual_port = listener.local_addr()?.port();
    info!("TCP transport listening on {}", listener.local_addr()?);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    info!("TCP connection from {peer}");
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        handle_tcp(stream, app).await;
                    });
                }
                Err(e) => {
                    warn!("TCP accept error: {e}");
                }
            }
        }
    });

    Ok(actual_port)
}

/// State for a single TCP-connected client. Mirrors `ClientState` in connection.rs
/// but uses a shared TCP writer instead of a QUIC `Connection` for sending.
struct TcpClientState {
    authenticated: bool,
    client_id: Option<ClientId>,
    connection_id: Option<ConnectionId>,
    capabilities: ClientCapabilities,
    attached: HashMap<String, AbortHandle>,
    /// Sender for the TCP writer task.
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    app: Arc<ServerApp>,
}

impl TcpClientState {
    fn new(app: Arc<ServerApp>, ctrl_tx: mpsc::UnboundedSender<ServerMessage>) -> Self {
        Self {
            authenticated: false,
            client_id: None,
            connection_id: None,
            capabilities: ClientCapabilities::default(),
            attached: HashMap::new(),
            ctrl_tx,
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
                    info!("TCP client {client_id:?} authenticated (conn={conn_id:?})");
                } else {
                    self.send(ServerMessage::AuthResult {
                        success: false,
                        reason: Some("invalid token".to_string()),
                        client_id: None,
                        server_version: None,
                        connection_id: None,
                    });
                    warn!("TCP authentication failed");
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
                        // For TCP, pane diffs are interleaved on the same stream
                        // via ctrl_tx (the shared writer). We forward them from
                        // client_rx into ctrl_tx in a background task.
                        let ctrl_tx = self.ctrl_tx.clone();
                        let pane_id_clone = pane_id.clone();
                        let handle = tokio::spawn(async move {
                            tcp_pane_forwarder(result, pane_id_clone, &mut client_rx, ctrl_tx)
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
                    debug!("TCP detached from pane '{pane_id}'");
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
                debug!("TCP client {client_id:?} snapshot mode = {enabled}");
            }

            ClientMessage::Ping { seq } => {
                self.send(ServerMessage::Pong { seq });
            }

            ClientMessage::Pong { .. } => {}
        }

        true
    }
}

impl Drop for TcpClientState {
    fn drop(&mut self) {
        for (_, handle) in self.attached.drain() {
            handle.abort();
        }
    }
}

/// Forward initial replay data + live diffs from a pane attach into ctrl_tx.
///
/// This is the TCP equivalent of `pane_uni_writer` in connection.rs.
/// Instead of a dedicated uni-stream, all messages are interleaved on the
/// shared TCP control stream (ctrl_tx). This works because all `ServerMessage`
/// variants carry `pane_id` for client-side demultiplexing.
async fn tcp_pane_forwarder(
    attach_result: AttachResult,
    pane_id: String,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => {
            let _ = ctrl_tx.send(ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            });
        }
        AttachResult::Delta(diffs) => {
            for (seqno, diff) in diffs {
                let _ = ctrl_tx.send(ServerMessage::TerminalUpdate {
                    pane_id: pane_id.clone(),
                    diff,
                    seqno,
                    sent_at_ms: epoch_millis(),
                });
            }
        }
        AttachResult::SyncReset(snapshot, seqno) => {
            let _ = ctrl_tx.send(ServerMessage::SyncReset {
                pane_id: pane_id.clone(),
            });
            let _ = ctrl_tx.send(ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            });
        }
    }

    while let Some(msg) = client_rx.recv().await {
        if ctrl_tx.send(msg).is_err() {
            break;
        }
    }
}

/// Handle a single TCP client connection.
async fn handle_tcp(stream: TcpStream, app: Arc<ServerApp>) {
    let (read_half, write_half): (ReadHalf<TcpStream>, WriteHalf<TcpStream>) =
        tokio::io::split(stream);

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Writer task: drain ctrl_rx and write frames to the TCP stream.
    let writer_task = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(msg) = ctrl_rx.recv().await {
            match encode_server(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut write_half, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("TCP encode error: {e}"),
            }
        }
        let _ = write_half.shutdown().await;
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

    let mut state = TcpClientState::new(app.clone(), ctrl_tx);
    let mut read_half = read_half;

    loop {
        match read_frame(&mut read_half).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => {
                    if !state.handle(client_msg).await {
                        break;
                    }
                }
                Err(e) => {
                    warn!("TCP decode error: {e}");
                    state.error(None, ErrorCode::InvalidMessage, e.to_string());
                }
            },
            Ok(None) => {
                debug!("TCP control stream closed");
                break;
            }
            Err(e) => {
                warn!("TCP read error: {e}");
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
    info!("TCP connection closed");
}

/// Translate a kmux-pty lifecycle event into a protocol `SessionEventMsg`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// Verify that the TCP listener binds successfully on port 0 (random).
    #[tokio::test]
    async fn tcp_listener_binds_random_port() {
        let app = Arc::new(ServerApp::new("test-token".to_string()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let port = serve_tcp(addr, app).await.expect("should bind");
        assert!(port > 0, "expected a non-zero port");
    }
}
