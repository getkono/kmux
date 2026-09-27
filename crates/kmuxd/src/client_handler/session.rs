use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kmux_protocol::TransportKind;
use kmux_protocol::messages::{ErrorCode, ServerMessage, SessionEventMsg, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame_compressed_into};

/// Cap on how many queued messages one flush coalesces. Bounds batch memory and
/// keeps flush latency tight under a sustained burst; the remainder stays queued
/// for the next recv.
pub(crate) const MAX_WRITE_BATCH: usize = 256;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{Instrument, Span, debug, error, info, warn};

use super::liveness::{self, Liveness, PING_INTERVAL};
use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
use crate::client_handler::{
    OutboundCompression, PaneAttacher, SharedClientState, handle_message, pty_event_to_msg,
};
use crate::outbound::{
    self, CloseReason, Closer, OUTBOUND_CAPACITY, OutboundRx, OutboundTx, close_channel,
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
            snapshot: Arc::new(snapshot),
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
                snapshot: Arc::new(snapshot),
                seqno,
                sent_at_ms: epoch_millis(),
            },
        ],
    }
}

/// The tasks a connection runs once it has authenticated: the session-event
/// and VT-event forwarders and the ping task. Aborted when dropped, so they
/// end with the connection.
struct AuthenticatedTasks([JoinHandle<()>; 3]);

impl Drop for AuthenticatedTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// VT events a flood of pane output can raise one per escape sequence: a
/// bell per BEL byte, a title or progress change per OSC. `cat` of a binary
/// file raises thousands, to every connection, attached to the pane or not.
fn is_lossy_vt_event(msg: &ServerMessage) -> bool {
    matches!(
        msg,
        ServerMessage::Event {
            event: SessionEventMsg::PaneBell { .. }
                | SessionEventMsg::PaneTitleChanged { .. }
                | SessionEventMsg::PaneProgressChanged { .. }
        }
    )
}

/// Queue a server-wide VT event for this connection. A flood-prone one goes
/// on the pane-data lane and is dropped when that is congested (issue #206):
/// on the never-drop control lane, a burst from one pane would fill the queue
/// and close every slow client's connection. Everything else (layout, tab
/// lifecycle, clipboard) is control.
pub(crate) fn forward_vt_event(out: &OutboundTx, msg: ServerMessage) {
    if is_lossy_vt_event(&msg) {
        let _ = out.try_send_data(msg);
    } else {
        let _ = out.send(msg);
    }
}

/// Forward one server-wide broadcast to this connection until the daemon
/// closes it, handing each item to `deliver`.
///
/// A lag means this connection missed events, and a missed event may have
/// been a session or tab created or closed, or a layout change. So the
/// forwarder does not end or skip ahead silently (issue #208): it sends a
/// fresh session list — every session with its tabs and layouts — that the
/// client reconciles against, then carries on.
async fn forward_broadcast<T: Clone>(
    mut rx: broadcast::Receiver<T>,
    app: Arc<ServerApp>,
    out: OutboundTx,
    deliver: impl Fn(&OutboundTx, T),
) {
    loop {
        match rx.recv().await {
            Ok(item) => deliver(&out, item),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                warn!(missed, "server events lagged; resyncing the session list");
                if out.send(app.session_list_resync().await).is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn spawn_authenticated_forwarders(
    app: &Arc<ServerApp>,
    ctrl_tx: OutboundTx,
    metrics: Arc<ConnectionMetrics>,
    conn_span: Span,
) -> AuthenticatedTasks {
    let event_task = tokio::spawn(
        forward_broadcast(
            app.subscribe_events(),
            Arc::clone(app),
            ctrl_tx.clone(),
            |out, event| {
                let _ = out.send(ServerMessage::Event {
                    event: pty_event_to_msg(event),
                });
            },
        )
        .instrument(conn_span.clone()),
    );

    let vt_task = tokio::spawn(
        forward_broadcast(
            app.subscribe_vt_events(),
            Arc::clone(app),
            ctrl_tx.clone(),
            forward_vt_event,
        )
        .instrument(conn_span.clone()),
    );

    let ping_tx = ctrl_tx;
    let ping_task = tokio::spawn(
        async move {
            let mut seq = 0u64;
            let mut interval = tokio::time::interval(PING_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                *metrics.last_ping_sent.lock().unwrap() = Some((seq, std::time::Instant::now()));
                // The pong clock starts when the writer puts the ping on the
                // wire (`write_batch`), not here: queued behind a log dump or
                // pane data, it may not go out for a while.
                if ping_tx.send(ServerMessage::Ping { seq }).is_err() {
                    break;
                }
                seq += 1;
            }
        }
        .instrument(conn_span),
    );

    AuthenticatedTasks([event_task, vt_task, ping_task])
}

/// Longest a single frame write, or a flush, may take before the connection
/// is closed (issue #206). A peer that stops reading — its TCP window at zero —
/// otherwise pins the writer task, and with it the connection, forever. Long
/// enough for a large snapshot over a slow but live link.
pub(crate) const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Drain a connection's outbound queue onto `writer` until every sender is
/// gone or a write fails, then shut the transport down.
///
/// Every frame write and every flush must finish within `write_timeout`. One
/// that does not closes the connection through `closer`
/// ([`CloseReason::WriteTimeout`]); so does a failed write
/// ([`CloseReason::WriteFailed`]), since nothing more can reach the client.
///
/// Batches all immediately-available messages into one flush. Each
/// `write_frame_compressed_into` writes a whole frame without flushing; a
/// single trailing flush then pushes the batch as far fewer TLS records /
/// syscalls than one flush per message. Per-frame compression and the
/// one-frame-per-message wire format are unchanged, so the bytes on the wire
/// are identical — only the flush boundaries move.
///
/// A `Ping` starts `liveness`'s pong clock once it is written, so the time a
/// ping spends queued behind other frames is not counted against the client.
async fn write_outbound<W: AsyncWrite + Unpin>(
    mut out_rx: OutboundRx,
    mut writer: W,
    metrics: Arc<ConnectionMetrics>,
    comp: Arc<OutboundCompression>,
    closer: Closer,
    liveness: Arc<Liveness>,
    write_timeout: Duration,
) {
    let mut batch: Vec<ServerMessage> = Vec::new();
    while let Some(first) = out_rx.recv().await {
        batch.clear();
        batch.push(first);
        for _ in 1..MAX_WRITE_BATCH {
            match out_rx.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        // The batch is off the queue: a lagged pane stream may resync.
        out_rx.mark_drained();
        if let Err(reason) = write_batch(
            &mut writer,
            &mut batch,
            &metrics,
            &comp,
            &liveness,
            write_timeout,
        )
        .await
        {
            closer.close(reason);
            break;
        }
    }
    // Shutting a TLS stream down writes close_notify, which a stalled peer
    // would block too.
    let _ = tokio::time::timeout(write_timeout, writer.shutdown()).await;
}

/// Write every message in `batch` as its own frame, then flush once. Each
/// write and the flush must finish within `write_timeout`. A written `Ping`
/// is recorded in `liveness`.
async fn write_batch<W: AsyncWrite + Unpin>(
    writer: &mut W,
    batch: &mut Vec<ServerMessage>,
    metrics: &ConnectionMetrics,
    comp: &OutboundCompression,
    liveness: &Liveness,
    write_timeout: Duration,
) -> Result<(), CloseReason> {
    for msg in batch.drain(..) {
        let bytes = match encode_server(&msg) {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!("encode error: {e}");
                continue;
            }
        };
        crate::capture::record(msg.category(), &bytes);
        // A failed write leaves only whole frames on the wire (each frame is
        // written in full before the next); the client reconnects + resyncs.
        let write = write_frame_compressed_into(&mut *writer, &bytes, comp.compressor());
        let wire_len = within(write_timeout, write).await?;
        if matches!(msg, ServerMessage::Ping { .. }) {
            liveness.on_ping_sent(tokio::time::Instant::now());
        }
        metrics
            .bytes_out
            .fetch_add(wire_len as u64, Ordering::Relaxed);
        // What the same frame would have cost uncompressed (length prefix +
        // codec tag + payload).
        metrics
            .bytes_out_uncompressed
            .fetch_add(5 + bytes.len() as u64, Ordering::Relaxed);
        metrics.msgs_out.fetch_add(1, Ordering::Relaxed);
    }
    within(write_timeout, kmux_protocol::flush(writer)).await
}

/// Await one write under `timeout`: a stall is [`CloseReason::WriteTimeout`],
/// a failure [`CloseReason::WriteFailed`].
pub(crate) async fn within<T, E>(
    timeout: Duration,
    write: impl Future<Output = Result<T, E>>,
) -> Result<T, CloseReason> {
    match tokio::time::timeout(timeout, write).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(_)) => Err(CloseReason::WriteFailed),
        Err(_) => {
            warn!(?timeout, "write timed out; peer is not reading");
            Err(CloseReason::WriteTimeout)
        }
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
/// `transport/remote/conn_id/client_id` fields).  It is cloned onto each spawned
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
    F: FnOnce(OutboundTx, Arc<OutboundCompression>) -> A,
{
    let metrics = Arc::new(ConnectionMetrics::new());

    // Outbound compression policy for this connection: level/min_size are the
    // configured constants; the auth handler flips it on once the daemon decides.
    let comp_out = Arc::new(OutboundCompression::new(
        app.compression.level,
        app.compression.min_size,
    ));

    let (closer, mut close_signal) = close_channel();
    let (ctrl_tx, out_rx) = outbound::channel(OUTBOUND_CAPACITY, closer.clone());

    // Closes the connection if it does not authenticate in time, or later
    // stops answering pings.
    let liveness = Arc::new(Liveness::new(tokio::time::Instant::now()));
    let watchdog_task = tokio::spawn(
        liveness::watchdog(Arc::clone(&liveness), closer.clone()).instrument(conn_span.clone()),
    );

    let mut writer_task = tokio::spawn(
        write_outbound(
            out_rx,
            writer,
            Arc::clone(&metrics),
            Arc::clone(&comp_out),
            closer,
            Arc::clone(&liveness),
            FRAME_WRITE_TIMEOUT,
        )
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
    let mut authenticated_tasks: Option<AuthenticatedTasks> = None;
    let mut flush_before_close = false;

    loop {
        // Every other branch closes the connection, so a read cancelled
        // mid-frame is never resumed.
        let frame = tokio::select! {
            frame = read_frame(&mut reader) => frame,
            reason = close_signal.closed() => {
                warn!(conn_id = ?state.connection_id.map(|c| c.0), ?reason, "closing connection");
                break;
            }
            // The writer only ends on its own when it has failed (and closed
            // the connection) or panicked. Either way nothing more reaches the
            // client, so a connection left open would be a silent one: no
            // replies, no pings, and so no pong deadline to end it.
            ended = &mut writer_task => {
                if let Err(e) = ended {
                    error!(conn_id = ?state.connection_id.map(|c| c.0), "outbound writer failed: {e}");
                }
                break;
            }
        };
        match frame {
            Ok(Some(data)) => {
                // Instrument inbound bytes on every frame, before auth. v1
                // clients send uncompressed uplink, so wire size is the 5-byte
                // header (length prefix + codec tag) plus the payload.
                metrics
                    .bytes_in
                    .fetch_add(5 + data.len() as u64, Ordering::Relaxed);
                metrics.msgs_in.fetch_add(1, Ordering::Relaxed);
                liveness.on_inbound();
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
                            liveness.on_authenticated();
                            authenticated_tasks = Some(spawn_authenticated_forwarders(
                                &app,
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

    watchdog_task.abort();
    drop(authenticated_tasks);

    let log_conn_id = state.connection_id.map(|c| c.0);
    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }
    if let Some(conn_id) = state.connection_id {
        app.unregister_client(conn_id).await;
    }

    drop(state);
    drop(attacher);
    // An ended writer (the select arm above) never sets `flush_before_close`,
    // so its finished handle is only aborted, a no-op, never awaited again.
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
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, FrontendKind, PROTOCOL_RANGE, protocol_capabilities,
    };
    use kmux_protocol::{decode_server, encode_client, write_frame};
    use kmux_sys::identity::Identity;

    use crate::fixtures::{NoopAttacher, fixture_app};

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
        let app = Arc::new(fixture_app());
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
        let app = Arc::new(fixture_app());
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

    /// A peer that stops reading fills its transport buffer, the frame write
    /// blocks, and after `FRAME_WRITE_TIMEOUT` the connection is closed and
    /// the writer ends. Before the timeout the writer waited forever.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_reading_is_closed_after_the_write_timeout() {
        // The client half is kept but never read, so writes block rather
        // than fail.
        let (server, _client) = tokio::io::duplex(64);
        let (closer, mut signal) = close_channel();
        let (out, out_rx) = outbound::channel(OUTBOUND_CAPACITY, closer.clone());
        let started = tokio::time::Instant::now();
        let writer = tokio::spawn(write_outbound(
            out_rx,
            server,
            Arc::new(ConnectionMetrics::new()),
            Arc::new(OutboundCompression::new(3, 1024)),
            closer,
            Arc::new(Liveness::new(tokio::time::Instant::now())),
            FRAME_WRITE_TIMEOUT,
        ));

        out.send(ServerMessage::LogChunk {
            request_id: 1,
            data: vec![0; 4096],
        })
        .expect("queued");

        let reason = tokio::time::timeout(FRAME_WRITE_TIMEOUT * 2, signal.closed()).await;
        assert_eq!(reason, Ok(CloseReason::WriteTimeout));
        assert!(started.elapsed() >= FRAME_WRITE_TIMEOUT);
        writer
            .await
            .expect("the writer ends instead of waiting forever");
    }

    /// A ping queued behind frames the client has not taken yet does not
    /// start the pong clock; writing it does. A client draining a large
    /// reply over a slow link is not closed for a ping it has not seen.
    #[tokio::test(start_paused = true)]
    async fn the_pong_clock_starts_when_the_ping_is_written_not_queued() {
        use liveness::{PONG_DEADLINE, Verdict};

        // Small enough that the first frame fills it and the ping waits.
        let (server, mut client) = tokio::io::duplex(64);
        let (closer, _signal) = close_channel();
        let (out, out_rx) = outbound::channel(OUTBOUND_CAPACITY, closer.clone());
        let liveness = Arc::new(Liveness::new(tokio::time::Instant::now()));
        liveness.on_authenticated();
        tokio::spawn(write_outbound(
            out_rx,
            server,
            Arc::new(ConnectionMetrics::new()),
            Arc::new(OutboundCompression::new(3, 1024)),
            closer,
            Arc::clone(&liveness),
            // Far beyond this test, so only the pong clock is in play.
            Duration::from_secs(24 * 3600),
        ));
        out.send(ServerMessage::LogChunk {
            request_id: 1,
            data: vec![0; 4096],
        })
        .unwrap();
        out.send(ServerMessage::Ping { seq: 0 }).unwrap();

        tokio::time::sleep(PONG_DEADLINE * 2).await;
        assert_eq!(
            liveness.verdict(tokio::time::Instant::now()),
            Verdict::Keep,
            "the ping is still queued"
        );

        // The client reads: the chunk and then the ping go out.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(10), read_frame(&mut client))
                .await
                .expect("a frame")
                .unwrap()
                .expect("the stream is open");
        }
        let written = tokio::time::Instant::now();
        assert_eq!(liveness.verdict(written + PONG_DEADLINE), Verdict::Keep);
        assert_eq!(
            liveness.verdict(written + PONG_DEADLINE + Duration::from_secs(1)),
            Verdict::Close(CloseReason::PongDeadline),
            "the clock started when the ping was written"
        );
    }

    /// A socket that connects and sends nothing is closed once the auth
    /// deadline passes; before it, such a socket held its tasks forever.
    #[tokio::test(start_paused = true)]
    async fn a_silent_unauthenticated_connection_is_closed_at_the_auth_deadline() {
        let app = Arc::new(fixture_app());
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, _client_write) = tokio::io::split(client);
        let started = tokio::time::Instant::now();

        let session = run_client_session(
            server_read,
            server_write,
            app,
            TransportKind::Uds,
            |_tx, _compression| NoopAttacher,
            tracing::info_span!("auth_deadline_test"),
        );
        tokio::time::timeout(liveness::AUTH_DEADLINE * 2, session)
            .await
            .expect("the session ends at the auth deadline");

        assert!(started.elapsed() > liveness::AUTH_DEADLINE);
        assert!(
            read_frame(&mut client_read).await.unwrap().is_none(),
            "the daemon closed the socket"
        );
    }

    /// Every queued message reaches the peer as its own frame, in order, and
    /// is counted; the writer ends cleanly once every sender is gone.
    #[tokio::test]
    async fn write_outbound_writes_each_frame_in_order_and_counts_it() {
        let (server, mut client) = tokio::io::duplex(64 * 1024);
        let (closer, _signal) = close_channel();
        let (out, out_rx) = outbound::channel(OUTBOUND_CAPACITY, closer.clone());
        let metrics = Arc::new(ConnectionMetrics::new());
        let writer = tokio::spawn(write_outbound(
            out_rx,
            server,
            Arc::clone(&metrics),
            Arc::new(OutboundCompression::new(3, 1024)),
            closer,
            Arc::new(Liveness::new(tokio::time::Instant::now())),
            FRAME_WRITE_TIMEOUT,
        ));
        let payload_len = encode_server(&ServerMessage::Pong { seq: 3 })
            .unwrap()
            .len() as u64;

        out.send(ServerMessage::Pong { seq: 3 }).unwrap();
        out.send(ServerMessage::Pong { seq: 4 }).unwrap();
        drop(out);
        writer.await.expect("the writer ends once its queue closes");

        for seq in [3, 4] {
            let frame = read_frame(&mut client).await.unwrap().expect("a frame");
            assert!(
                matches!(decode_server(&frame).unwrap(), ServerMessage::Pong { seq: s } if s == seq)
            );
        }
        assert!(
            read_frame(&mut client).await.unwrap().is_none(),
            "then shutdown"
        );
        assert_eq!(metrics.msgs_out.load(Ordering::Relaxed), 2);
        // Uncompressed: a 4-byte length prefix and a 1-byte codec tag each.
        assert_eq!(
            metrics.bytes_out.load(Ordering::Relaxed),
            2 * (5 + payload_len)
        );
        assert_eq!(
            metrics.bytes_out_uncompressed.load(Ordering::Relaxed),
            2 * (5 + payload_len)
        );
    }

    /// Read frames until `want` picks one out. Bounded, so a session that
    /// stops sending fails the test instead of hanging it (on the paused
    /// clock the bound passes as soon as nothing else is scheduled).
    async fn read_until<T>(
        reader: &mut (impl AsyncRead + Unpin),
        want: impl Fn(ServerMessage) -> Option<T>,
    ) -> T {
        let read = async {
            loop {
                let frame = read_frame(&mut *reader)
                    .await
                    .unwrap()
                    .expect("the session is open");
                if let Some(found) = want(decode_server(&frame).unwrap()) {
                    return found;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), read)
            .await
            .expect("the awaited frame within the bound")
    }

    /// Once authenticated, a client is pinged every `PING_INTERVAL`; one that
    /// never answers is closed `PONG_DEADLINE` after the first unanswered
    /// ping, and the connection's forwarding tasks end with it.
    #[tokio::test(start_paused = true)]
    async fn an_authenticated_client_that_never_answers_a_ping_is_closed() {
        let app = Arc::new(fixture_app());
        let (server, client) = tokio::io::duplex(1024 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let session = tokio::spawn(run_client_session(
            server_read,
            server_write,
            Arc::clone(&app),
            TransportKind::Uds,
            |_tx, _compression| NoopAttacher,
            tracing::info_span!("pong_deadline_test"),
        ));

        let identity = Identity::generate();
        let mut hello = auth(crate::fixtures::FIXTURE_TOKEN);
        if let ClientMessage::Auth { public_key, .. } = &mut hello {
            *public_key = identity.public_key_bytes().to_vec();
        }
        write_frame(&mut client_write, &encode_client(&hello).unwrap())
            .await
            .unwrap();
        let nonce = read_until(&mut client_read, |msg| match msg {
            ServerMessage::AuthChallenge { nonce } => Some(nonce),
            _ => None,
        })
        .await;
        let proof = ClientMessage::AuthProof {
            signature: identity.sign(&nonce),
        };
        write_frame(&mut client_write, &encode_client(&proof).unwrap())
            .await
            .unwrap();
        let accepted = read_until(&mut client_read, |msg| match msg {
            ServerMessage::AuthResult { success, .. } => Some(success),
            _ => None,
        })
        .await;
        assert!(accepted, "the handshake succeeds");

        let pinged = |msg| match msg {
            ServerMessage::Ping { seq } => Some(seq),
            _ => None,
        };
        assert_eq!(read_until(&mut client_read, pinged).await, 0);
        let first_ping = tokio::time::Instant::now();
        assert_eq!(read_until(&mut client_read, pinged).await, 1);

        tokio::time::timeout(liveness::PONG_DEADLINE * 2, session)
            .await
            .expect("the session ends at the pong deadline")
            .unwrap();
        assert!(first_ping.elapsed() > liveness::PONG_DEADLINE);
        // Sleeps, not yields: a yield loop never lets the paused clock
        // advance, so the bound would never pass.
        tokio::time::timeout(Duration::from_secs(10), async {
            while app.vt_subscriber_count() > 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the connection's forwarders ended with it");
    }

    /// Flood-prone VT events are dropped when pane data is congested, while
    /// other server-wide events still take the control lane.
    #[tokio::test]
    async fn forward_vt_event_drops_only_flood_prone_events_under_congestion() {
        let (out, _rx) = outbound::channel(8, close_channel().0);
        // 8 slots, 2 kept for control: fill the data share with control.
        for seq in 0..6 {
            out.send(ServerMessage::Pong { seq }).unwrap();
        }
        let pane_event = |event| ServerMessage::Event { event };
        let pane_id = "eagle/0".to_string();

        forward_vt_event(
            &out,
            pane_event(SessionEventMsg::PaneBell {
                pane_id: pane_id.clone(),
            }),
        );
        forward_vt_event(
            &out,
            pane_event(SessionEventMsg::PaneTitleChanged {
                pane_id: pane_id.clone(),
                title: "t".to_string(),
            }),
        );
        assert_eq!(out.len(), 6, "a bell and a title are dropped");

        forward_vt_event(&out, pane_event(SessionEventMsg::PaneClosed { pane_id }));
        assert_eq!(out.len(), 7, "a lifecycle event is control");
    }

    /// A subscriber that falls behind the server-wide broadcast is sent a
    /// resync session list and keeps forwarding. Before, a lag ended the
    /// session-event forwarder for good and the VT one skipped ahead silently.
    #[tokio::test(start_paused = true)]
    async fn a_lagged_forwarder_resyncs_the_session_list_and_keeps_running() {
        use kmux_protocol::messages::RESYNC_REQUEST_ID;

        let app = Arc::new(fixture_app());
        // One (federated) session, so the resync has something to list.
        let (_upstream, _peer) = app.install_channel_peer("fedlocal", "fedremote");
        let (out, mut out_rx) = outbound::channel(OUTBOUND_CAPACITY, close_channel().0);
        let rx = app.subscribe_vt_events();
        let renamed = |n: usize| ServerMessage::SessionRenamed {
            word_id: "eagle".to_string(),
            new_name: n.to_string(),
        };
        // More than the broadcast holds before the forwarder reads any: the
        // oldest are lost to it.
        for n in 0..600 {
            app.broadcast(renamed(n));
        }
        let forwarder = tokio::spawn(forward_broadcast(
            rx,
            Arc::clone(&app),
            out,
            forward_vt_event,
        ));

        let first = tokio::time::timeout(Duration::from_secs(10), out_rx.recv())
            .await
            .expect("the forwarder answers the lag")
            .expect("the queue is open");
        let ServerMessage::SessionListResult {
            request_id,
            sessions,
        } = first
        else {
            panic!("expected a resync session list, got {first:?}");
        };
        assert_eq!(request_id, RESYNC_REQUEST_ID);
        let listed: Vec<&str> = sessions.iter().map(|e| e.meta.word_id.as_str()).collect();
        assert_eq!(listed, vec!["fedlocal"]);
        // The newest events still follow, and so does one sent afterwards.
        app.broadcast(renamed(600));
        let mut last = None;
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(10), out_rx.recv()).await
        {
            let ServerMessage::SessionRenamed { new_name, .. } = msg else {
                panic!("expected only renames after the resync, got {msg:?}");
            };
            last = Some(new_name);
            if last.as_deref() == Some("600") {
                break;
            }
        }
        assert_eq!(last.as_deref(), Some("600"));
        assert!(!forwarder.is_finished(), "the forwarder is still running");
    }

    /// A transport whose every write panics, as a bug in the write path would.
    struct PanickingWriter;

    impl AsyncWrite for PanickingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            panic!("write path bug");
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A writer that dies without closing the connection (a panic) ends the
    /// session at once. Before, nothing more reached the client, not even a
    /// ping, so an authenticated connection it left open had no pong
    /// deadline to close it; here, unauthenticated, only the auth deadline
    /// would have.
    #[tokio::test(start_paused = true)]
    async fn a_writer_that_dies_ends_the_session() {
        let app = Arc::new(fixture_app());
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, _server_write) = tokio::io::split(server);
        let (_client_read, mut client_write) = tokio::io::split(client);
        let session = tokio::spawn(run_client_session(
            server_read,
            PanickingWriter,
            app,
            TransportKind::Uds,
            |_tx, _compression| NoopAttacher,
            tracing::info_span!("writer_death_test"),
        ));

        // Any reply sets the writer off.
        write_frame(&mut client_write, b"not a message")
            .await
            .unwrap();
        tokio::time::timeout(liveness::AUTH_DEADLINE / 2, session)
            .await
            .expect("the session ends with its writer, not at a deadline")
            .unwrap();
    }
}
