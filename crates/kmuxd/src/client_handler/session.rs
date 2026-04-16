use std::sync::Arc;
use std::time::Duration;

use kmux_protocol::messages::{ErrorCode, ServerMessage, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, SharedClientState, handle_message, pty_event_to_msg};

/// Build the initial replay messages for a pane attach result.
///
/// Shared by QUIC (`pane_uni_writer`) and TCP (`TcpAttacher`).
/// Both transports iterate the returned messages and send them through
/// their respective channels.
pub fn build_attach_replay(attach_result: AttachResult, pane_id: &str) -> Vec<ServerMessage> {
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => vec![ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot,
            seqno,
            sent_at_ms: epoch_millis(),
        }],
        AttachResult::Delta(diffs) => diffs
            .into_iter()
            .map(|(seqno, diff)| ServerMessage::TerminalUpdate {
                pane_id: pane_id.to_string(),
                diff,
                seqno,
                sent_at_ms: epoch_millis(),
            })
            .collect(),
        AttachResult::SyncReset(snapshot, seqno) => vec![
            ServerMessage::SyncReset {
                pane_id: pane_id.to_string(),
            },
            ServerMessage::TerminalSnapshot {
                pane_id: pane_id.to_string(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            },
        ],
    }
}

/// Generic client session handler shared by QUIC and TCP connections.
///
/// Runs the event-forwarder, ping, writer, and read-dispatch loop that are
/// identical for both transports.  Transport-specific setup (accepting a QUIC
/// bi-stream, splitting a TCP stream) is done by the caller before invoking
/// this function.
///
/// `make_attacher` is called with a clone of the control channel sender so
/// that transport-specific attachers (e.g. `TcpAttacher`) can share the
/// same output channel as the writer task.
///
/// `transport_label` is prepended to log messages (e.g. `""` for QUIC,
/// `"TCP "` for TCP).
pub async fn run_client_session<R, W, A, F>(
    mut reader: R,
    writer: W,
    app: Arc<ServerApp>,
    make_attacher: F,
    transport_label: &'static str,
) where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    A: PaneAttacher,
    F: FnOnce(mpsc::UnboundedSender<ServerMessage>) -> A,
{
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer_task = tokio::spawn(async move {
        let mut ctrl_rx = ctrl_rx;
        let mut writer = writer;
        while let Some(msg) = ctrl_rx.recv().await {
            match encode_server(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut writer, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("{transport_label}encode error: {e}"),
            }
        }
        let _ = writer.shutdown().await;
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

    let attacher = make_attacher(ctrl_tx.clone());
    let mut state = SharedClientState::new(app.clone(), ctrl_tx, transport_label);

    loop {
        match read_frame(&mut reader).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => {
                    if !handle_message(&mut state, client_msg, &attacher).await {
                        break;
                    }
                }
                Err(e) => {
                    warn!("{transport_label}decode error: {e}");
                    state.error(None, ErrorCode::InvalidMessage, e.to_string());
                }
            },
            Ok(None) => {
                debug!("{transport_label}control stream closed");
                break;
            }
            Err(e) => {
                warn!("{transport_label}read error: {e}");
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
    info!("{transport_label}connection closed");
}
