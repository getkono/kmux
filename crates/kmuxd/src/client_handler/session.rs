use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kmux_protocol::TransportKind;
use kmux_protocol::messages::{ErrorCode, ServerMessage, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{Instrument, Span, debug, info, warn};

use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
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
/// `conn_span` is the per-connection tracing span (created by the caller with
/// transport/remote/conn_id/client_id fields).  It is cloned onto each spawned
/// task so that every log line carries the connection context.  The caller must
/// also `.instrument(conn_span)` the returned future so the main loop itself
/// runs within the span.
pub async fn run_client_session<R, W, A, F>(
    mut reader: R,
    writer: W,
    app: Arc<ServerApp>,
    transport: TransportKind,
    make_attacher: F,
    conn_span: Span,
) where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    A: PaneAttacher,
    F: FnOnce(mpsc::UnboundedSender<ServerMessage>) -> A,
{
    let metrics = Arc::new(ConnectionMetrics::new());

    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer_metrics = Arc::clone(&metrics);
    let writer_task = tokio::spawn(
        async move {
            let mut ctrl_rx = ctrl_rx;
            let mut writer = writer;
            while let Some(msg) = ctrl_rx.recv().await {
                match encode_server(&msg) {
                    Ok(bytes) => {
                        if write_frame(&mut writer, &bytes).await.is_err() {
                            break;
                        }
                        // 4-byte length prefix + payload
                        writer_metrics
                            .bytes_out
                            .fetch_add(4 + bytes.len() as u64, Ordering::Relaxed);
                        writer_metrics.msgs_out.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => warn!("encode error: {e}"),
                }
            }
            let _ = writer.shutdown().await;
        }
        .instrument(conn_span.clone()),
    );

    let mut event_rx = app.subscribe_events();
    let event_tx = ctrl_tx.clone();
    let event_task = tokio::spawn(
        async move {
            while let Ok(event) = event_rx.recv().await {
                let msg = pty_event_to_msg(event);
                let _ = event_tx.send(ServerMessage::Event { event: msg });
            }
        }
        .instrument(conn_span.clone()),
    );

    let ping_tx = ctrl_tx.clone();
    let ping_metrics = Arc::clone(&metrics);
    let ping_task = tokio::spawn(
        async move {
            let mut seq = 0u64;
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                // Record send time before enqueueing so RTT calculation is not
                // inflated by writer-task queue depth.
                *ping_metrics.last_ping_sent.lock().unwrap() =
                    Some((seq, std::time::Instant::now()));
                if ping_tx.send(ServerMessage::Ping { seq }).is_err() {
                    break;
                }
                seq += 1;
            }
        }
        .instrument(conn_span.clone()),
    );

    let attacher = make_attacher(ctrl_tx.clone());
    let mut state = SharedClientState::new(
        app.clone(),
        ctrl_tx,
        conn_span,
        transport,
        Arc::clone(&metrics),
    );

    loop {
        match read_frame(&mut reader).await {
            Ok(Some(data)) => {
                // Instrument inbound bytes on every frame, before auth.
                metrics
                    .bytes_in
                    .fetch_add(4 + data.len() as u64, Ordering::Relaxed);
                metrics.msgs_in.fetch_add(1, Ordering::Relaxed);
                metrics
                    .last_activity_ms
                    .store(epoch_millis(), Ordering::Relaxed);

                match decode_client(&data) {
                    Ok(client_msg) => {
                        if !handle_message(&mut state, client_msg, &attacher).await {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(conn_id = ?state.connection_id.map(|c| c.0), "decode error: {e}");
                        state.error(None, ErrorCode::InvalidMessage, e.to_string());
                    }
                }
            }
            Ok(None) => {
                debug!(conn_id = ?state.connection_id.map(|c| c.0), "control stream closed");
                break;
            }
            Err(e) => {
                warn!(conn_id = ?state.connection_id.map(|c| c.0), "read error: {e}");
                break;
            }
        }
    }

    event_task.abort();
    ping_task.abort();

    let log_conn_id = state.connection_id.map(|c| c.0);
    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }
    if let Some(conn_id) = state.connection_id {
        app.unregister_client(conn_id).await;
    }

    drop(state);
    writer_task.abort();
    info!(conn_id = ?log_conn_id, "connection closed");
}
