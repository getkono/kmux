use std::sync::Arc;
use std::time::{Duration, Instant};

use kmux_protocol::messages::{ErrorCode, ServerMessage, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use quinn::Connection;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, SharedClientState, handle_message, pty_event_to_msg};

pub fn classify_error(e: &kmux_pty::error::KmuxError) -> ErrorCode {
    match e {
        kmux_pty::error::KmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        kmux_pty::error::KmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
        kmux_pty::error::KmuxError::Pty(err) if *err == nix::Error::EPERM => ErrorCode::InputLocked,
        _ => ErrorCode::InternalError,
    }
}

// ─── QUIC-specific PaneAttacher ───────────────────────────────────────────────

/// Streams pane diffs to the client over a QUIC unidirectional stream.
struct QuicAttacher {
    conn: Connection,
}

impl PaneAttacher for QuicAttacher {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        mut client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl std::future::Future<Output = Result<AbortHandle, String>> + Send {
        let conn = self.conn.clone();
        async move {
            let uni_stream = conn
                .open_uni()
                .await
                .map_err(|e| format!("failed to open uni stream: {e}"))?;
            let handle = tokio::spawn(async move {
                pane_uni_writer(uni_stream, result, pane_id, &mut client_rx).await;
            })
            .abort_handle();
            Ok(handle)
        }
    }
}

// ─── QUIC pane stream writer ──────────────────────────────────────────────────

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

// ─── QUIC connection handler ──────────────────────────────────────────────────

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

    let attacher = QuicAttacher { conn: conn.clone() };
    let mut state = SharedClientState::new(app.clone(), ctrl_tx, "");

    loop {
        match read_frame(&mut ctrl_recv).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => {
                    if !handle_message(&mut state, client_msg, &attacher).await {
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
