use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kmux_protocol::TransportKind;
use kmux_protocol::messages::{ErrorCode, ServerMessage, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame_compressed_into};

/// Cap on how many queued messages one flush coalesces. Bounds batch memory and
/// keeps flush latency tight under a sustained burst; the remainder stays queued
/// for the next recv.
pub(crate) const MAX_WRITE_BATCH: usize = 256;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::{Instrument, Span, debug, info, warn};

use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
use crate::client_handler::{
    OutboundCompression, PaneAttacher, SharedClientState, handle_message, pty_event_to_msg,
};

/// Build the initial replay messages for a pane attach result.
///
/// Shared by QUIC (`pane_uni_writer`) and TCP (`TcpAttacher`).
/// Both transports iterate the returned messages and send them through
/// their respective channels.
pub fn build_attach_replay(attach_result: AttachResult, pane_id: &str) -> Vec<ServerMessage> {
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => vec![ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot: std::sync::Arc::new(snapshot),
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
                snapshot: std::sync::Arc::new(snapshot),
                seqno,
                sent_at_ms: epoch_millis(),
            },
        ],
    }
}

fn spawn_authenticated_forwarders(
    app: Arc<ServerApp>,
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    metrics: Arc<ConnectionMetrics>,
    conn_span: Span,
) -> (JoinHandle<()>, JoinHandle<()>, JoinHandle<()>) {
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

    let mut vt_rx = app.subscribe_vt_events();
    let vt_tx = ctrl_tx.clone();
    let vt_task = tokio::spawn(
        async move {
            loop {
                match vt_rx.recv().await {
                    Ok(msg) => {
                        let _ = vt_tx.send(msg);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        .instrument(conn_span.clone()),
    );

    let ping_tx = ctrl_tx;
    let ping_task = tokio::spawn(
        async move {
            let mut seq = 0u64;
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                *metrics.last_ping_sent.lock().unwrap() = Some((seq, std::time::Instant::now()));
                if ping_tx.send(ServerMessage::Ping { seq }).is_err() {
                    break;
                }
                seq += 1;
            }
        }
        .instrument(conn_span),
    );

    (event_task, vt_task, ping_task)
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
    F: FnOnce(mpsc::UnboundedSender<ServerMessage>, Arc<OutboundCompression>) -> A,
{
    let metrics = Arc::new(ConnectionMetrics::new());

    // Outbound compression policy for this connection: level/min_size are the
    // configured constants; the auth handler flips it on once the daemon decides.
    let comp_out = Arc::new(OutboundCompression::new(
        app.compression.level,
        app.compression.min_size,
    ));

    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer_metrics = Arc::clone(&metrics);
    let writer_comp = Arc::clone(&comp_out);
    let mut writer_task = tokio::spawn(
        async move {
            let mut ctrl_rx = ctrl_rx;
            let mut writer = writer;
            // Batch all immediately-available messages into one flush. Each
            // `write_frame_compressed_into` writes a whole frame without
            // flushing; a single trailing flush then pushes the batch as far
            // fewer TLS records / syscalls than the old one-flush-per-message
            // loop. Per-frame compression and the one-frame-per-message wire
            // format are unchanged, so the bytes on the wire are identical —
            // only the flush boundaries move.
            let mut batch: Vec<ServerMessage> = Vec::new();
            'writer: while let Some(first) = ctrl_rx.recv().await {
                batch.clear();
                batch.push(first);
                while batch.len() < MAX_WRITE_BATCH {
                    match ctrl_rx.try_recv() {
                        Ok(m) => batch.push(m),
                        Err(_) => break,
                    }
                }
                for msg in batch.drain(..) {
                    match encode_server(&msg) {
                        Ok(bytes) => {
                            crate::capture::record(msg.category(), &bytes);
                            match write_frame_compressed_into(
                                &mut writer,
                                &bytes,
                                writer_comp.compressor(),
                            )
                            .await
                            {
                                Ok(wire_len) => {
                                    writer_metrics
                                        .bytes_out
                                        .fetch_add(wire_len as u64, Ordering::Relaxed);
                                    // What the same frame would have cost uncompressed
                                    // (length prefix + codec tag + payload).
                                    writer_metrics
                                        .bytes_out_uncompressed
                                        .fetch_add(5 + bytes.len() as u64, Ordering::Relaxed);
                                    writer_metrics.msgs_out.fetch_add(1, Ordering::Relaxed);
                                }
                                // A write failure leaves only whole frames on the
                                // wire (each frame is written in full before the
                                // next); the client reconnects + resyncs.
                                Err(_) => break 'writer,
                            }
                        }
                        Err(e) => warn!("encode error: {e}"),
                    }
                }
                if kmux_protocol::flush(&mut writer).await.is_err() {
                    break;
                }
            }
            let _ = writer.shutdown().await;
        }
        .instrument(conn_span.clone()),
    );

    let attacher = make_attacher(ctrl_tx.clone(), Arc::clone(&comp_out));
    let mut state = SharedClientState::new(
        app.clone(),
        ctrl_tx,
        conn_span,
        transport,
        Arc::clone(&metrics),
        comp_out,
    );
    let mut authenticated_tasks: Option<(JoinHandle<()>, JoinHandle<()>, JoinHandle<()>)> = None;
    let mut flush_before_close = false;

    loop {
        match read_frame(&mut reader).await {
            Ok(Some(data)) => {
                // Instrument inbound bytes on every frame, before auth. v1
                // clients send uncompressed uplink, so wire size is the 5-byte
                // header (length prefix + codec tag) plus the payload.
                metrics
                    .bytes_in
                    .fetch_add(5 + data.len() as u64, Ordering::Relaxed);
                metrics.msgs_in.fetch_add(1, Ordering::Relaxed);
                metrics
                    .last_activity_ms
                    .store(epoch_millis(), Ordering::Relaxed);

                match decode_client(&data) {
                    Ok(client_msg) => {
                        let was_authenticated = state.authenticated;
                        if !handle_message(&mut state, client_msg, &attacher).await {
                            debug_assert!(!state.authenticated);
                            flush_before_close = true;
                            break;
                        }
                        if !was_authenticated && state.authenticated {
                            authenticated_tasks = Some(spawn_authenticated_forwarders(
                                app.clone(),
                                state.ctrl_tx.clone(),
                                Arc::clone(&metrics),
                                state.conn_span.clone(),
                            ));
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

    if let Some((event_task, vt_task, ping_task)) = authenticated_tasks {
        event_task.abort();
        ping_task.abort();
        vt_task.abort();
    }

    let log_conn_id = state.connection_id.map(|c| c.0);
    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }
    if let Some(conn_id) = state.connection_id {
        app.unregister_client(conn_id).await;
    }

    drop(state);
    drop(attacher);
    if flush_before_close {
        // Authentication failures carry a useful AuthResult reason. Close the
        // channel senders and give the writer a bounded opportunity to flush
        // that frame before shutting down the transport.
        if tokio::time::timeout(Duration::from_secs(1), &mut writer_task)
            .await
            .is_err()
        {
            writer_task.abort();
        }
    } else {
        writer_task.abort();
    }
    info!(conn_id = ?log_conn_id, "connection closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::identity::Identity;
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, FrontendKind, PROTOCOL_RANGE, protocol_capabilities,
    };
    use kmux_protocol::{decode_server, encode_client, write_frame};
    use tokio::task::AbortHandle;

    struct NoopAttacher;

    impl PaneAttacher for NoopAttacher {
        async fn start_pane_stream(
            &self,
            _pane_id: String,
            _result: AttachResult,
            _client_rx: mpsc::Receiver<ServerMessage>,
        ) -> Result<AbortHandle, String> {
            Err("not used in session tests".to_string())
        }
    }

    fn auth(token: &str) -> ClientMessage {
        let identity = Identity::generate();
        ClientMessage::Auth {
            token: token.to_string(),
            protocol_range: PROTOCOL_RANGE,
            protocol_capabilities: protocol_capabilities(),
            capabilities: ClientCapabilities::default(),
            connection_id: None,
            public_key: identity.public_key_bytes().to_vec(),
            hostname: "host".to_string(),
            username: "user".to_string(),
            client_kind: FrontendKind::Cli,
            client_git_sha: String::new(),
            client_git_dirty: false,
            client_build_profile: String::new(),
        }
    }

    #[tokio::test]
    async fn unauthenticated_session_emits_no_events_or_ping() {
        let app = Arc::new(ServerApp::new("expected".to_string()));
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let session = tokio::spawn(run_client_session(
            server_read,
            server_write,
            app,
            TransportKind::Uds,
            |_tx, _compression| NoopAttacher,
            tracing::info_span!("pre_auth_test"),
        ));

        assert!(
            tokio::time::timeout(Duration::from_millis(100), read_frame(&mut client_read))
                .await
                .is_err(),
            "nothing may be forwarded before authentication"
        );

        client_write.shutdown().await.unwrap();
        session.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_token_result_is_flushed_before_close() {
        let app = Arc::new(ServerApp::new("expected".to_string()));
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let session = tokio::spawn(run_client_session(
            server_read,
            server_write,
            app,
            TransportKind::Uds,
            |_tx, _compression| NoopAttacher,
            tracing::info_span!("auth_reject_test"),
        ));

        let bytes = encode_client(&auth("wrong")).unwrap();
        write_frame(&mut client_write, &bytes).await.unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut client_read))
            .await
            .expect("auth rejection should arrive promptly")
            .unwrap()
            .expect("auth rejection frame");
        let message = decode_server(&frame).unwrap();
        assert!(matches!(
            message,
            ServerMessage::AuthResult {
                success: false,
                reason: Some(ref reason),
                ..
            } if reason == "invalid token"
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), read_frame(&mut client_read))
                .await
                .expect("server should close after the rejection")
                .unwrap()
                .is_none()
        );

        session.await.unwrap();
    }
}
