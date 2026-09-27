use std::sync::Arc;
use std::time::{Duration, Instant};

use kmux_protocol::messages::{ErrorCode, ServerMessage};
use kmux_protocol::{Compressor, encode_server, write_frame_compressed_into};
use quinn::Connection;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{Instrument, debug};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{
    MAX_WRITE_BATCH, OutboundCompression, PaneAttacher, build_attach_replay, run_client_session,
    within,
};
use crate::outbound::{CloseReason, OutboundTx};

pub fn classify_error(e: &kmux_pty::error::KmuxError) -> ErrorCode {
    match e {
        kmux_pty::error::KmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        kmux_pty::error::KmuxError::PaneNotFound { .. } => ErrorCode::PaneNotFound,
        kmux_pty::error::KmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
        kmux_pty::error::KmuxError::SessionLimit { .. } => ErrorCode::SessionLimitReached,
        kmux_pty::error::KmuxError::Pty(err) if *err == nix::Error::EPERM => ErrorCode::InputLocked,
        _ => ErrorCode::InternalError,
    }
}

// ─── QUIC-specific PaneAttacher ───────────────────────────────────────────────

/// Streams pane diffs to the client over a QUIC unidirectional stream.
struct QuicAttacher {
    conn: Connection,
    /// Shared outbound compression policy for this connection's pane streams.
    comp_out: Arc<OutboundCompression>,
    /// The connection's control channel, where a stalled pane's resync goes.
    ctrl_tx: OutboundTx,
    /// How long a pane stream may refuse frames before it is reset:
    /// [`PANE_STREAM_STALL_TIMEOUT`] outside tests.
    stall_timeout: Duration,
}

impl PaneAttacher for QuicAttacher {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        mut client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl Future<Output = Result<AbortHandle, String>> + Send {
        let conn = self.conn.clone();
        let comp_out = Arc::clone(&self.comp_out);
        let ctrl_tx = self.ctrl_tx.clone();
        let stall_timeout = self.stall_timeout;
        async move {
            let mut uni_stream = conn
                .open_uni()
                .await
                .map_err(|e| format!("failed to open uni stream: {e}"))?;
            let handle = tokio::spawn(async move {
                let written = pane_uni_writer(
                    &mut uni_stream,
                    result,
                    &pane_id,
                    &mut client_rx,
                    &comp_out,
                    stall_timeout,
                )
                .await;
                if let Some(resync) = stall_resync(written, &pane_id) {
                    // Only this stream: the connection and its other panes
                    // carry on. The pane's queue closes with this task, so
                    // the relay drops it until the client re-attaches.
                    let _ = uni_stream.reset(PANE_STREAM_STALLED_CODE.into());
                    let _ = ctrl_tx.send(resync);
                }
            })
            .abort_handle();
            Ok(handle)
        }
    }
}

// ─── QUIC pane stream writer ──────────────────────────────────────────────────

/// Longest a QUIC pane stream may refuse frames before it is reset
/// (issue #207): 30 s past the QUIC idle timeout.
///
/// A pane stream stops taking frames when the client stops reading it — or
/// when nothing reaches the client at all: a laptop asleep, a network gone.
/// The second is the connection's business, not the stream's: with keep-alives
/// every 15 s the connection outlives any outage shorter than its idle
/// timeout, and past that it closes, failing every stream's write at once.
/// Holding a stream at least that long means a stall reset never fires during
/// an outage the connection would have ridden out, so a network drop or a
/// sleep costs no pane a resync. What is left is a client that is reachable —
/// it answers keep-alives — but has not read this stream for five and a half
/// minutes: that stream alone is reset.
pub(crate) const PANE_STREAM_STALL_TIMEOUT: Duration =
    Duration::from_secs(kmux_sys::QUIC_IDLE_TIMEOUT_SECS + 30);

/// QUIC application error code a stalled pane stream is reset with.
const PANE_STREAM_STALLED_CODE: u32 = 1;

/// What the client is told when its pane stream ended as `written`: for a
/// stall, the stream is reset and the client gets a `Lagged` on the control
/// stream, which it answers by re-attaching the pane — the existing
/// `SyncReset` + snapshot resync, on a fresh stream (issue #207). A stream
/// that ended or failed on its own needs no resync: either the pane closed or
/// the connection is going.
fn stall_resync(written: Result<(), CloseReason>, pane_id: &str) -> Option<ServerMessage> {
    (written == Err(CloseReason::WriteTimeout)).then(|| ServerMessage::Lagged {
        pane_id: pane_id.to_string(),
        missed_count: 1,
    })
}

/// Write initial replay data + live diffs on a server-initiated unidirectional
/// stream, until the pane's queue closes or a write fails.
///
/// Every frame write and every flush must finish within `write_timeout`
/// (issue #207): a client that stops reading a pane stream fills its QUIC
/// flow-control window, and the write would otherwise wait forever, pinning
/// this task and the pane's queue. Such a stall is
/// [`CloseReason::WriteTimeout`]; a failed write is
/// [`CloseReason::WriteFailed`].
async fn pane_uni_writer<W: AsyncWrite + Unpin>(
    uni: &mut W,
    attach_result: AttachResult,
    pane_id: &str,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
    comp_out: &OutboundCompression,
    write_timeout: Duration,
) -> Result<(), CloseReason> {
    // Replay frames are written as whole frames then flushed once.
    for msg in build_attach_replay(attach_result, pane_id) {
        within(write_timeout, write_frame(uni, &msg, comp_out.compressor())).await?;
    }
    within(write_timeout, kmux_protocol::flush(uni)).await?;

    // Network impairment shim (issue #72): per-pane jitter on live pane-data
    // frames only. `None` (no env knobs) skips the delay entirely.
    let impair = crate::impair::config();
    let mut rng = impair.map(|c| c.rng_for(crate::impair::pane_salt(pane_id)));

    // Batch all immediately-available frames into one flush, mirroring the
    // merged TCP/UDS writer (`run_client_session`). On QUIC the per-stream flush
    // is cheap, so this mainly drops per-message await overhead and keeps the
    // writer paths uniform; the bytes on the wire are unchanged.
    let mut batch: Vec<ServerMessage> = Vec::new();
    while let Some(first) = client_rx.recv().await {
        batch.clear();
        batch.push(first);
        while batch.len() < MAX_WRITE_BATCH {
            match client_rx.try_recv() {
                Ok(m) => batch.push(m),
                Err(_) => break,
            }
        }
        for msg in batch.drain(..) {
            if let (Some(cfg), Some(rng)) = (impair, rng.as_mut()) {
                crate::impair::maybe_delay(cfg, msg.category(), rng).await;
            }
            let write_start = Instant::now();
            within(write_timeout, write_frame(uni, &msg, comp_out.compressor())).await?;
            let write_us = write_start.elapsed().as_micros();
            if write_us > 1000 {
                debug!(pane_id, write_us, "slow uni stream write");
            }
        }
        within(write_timeout, kmux_protocol::flush(uni)).await?;
    }

    // Finish the stream (a QUIC `FIN`). quinn queues it and returns at once;
    // it never waits for the peer, so this needs no timeout.
    let _ = uni.shutdown().await;
    Ok(())
}

/// Write one whole frame to the stream WITHOUT flushing. A trailing
/// [`kmux_protocol::flush`] pushes a drained batch in one go.
async fn write_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    msg: &ServerMessage,
    comp: Compressor,
) -> Result<(), kmux_protocol::ProtocolError> {
    let bytes = encode_server(msg)?;
    if bytes.len() > 4096 {
        debug!(frame_bytes = bytes.len(), "large frame");
    }
    crate::capture::record(msg.category(), &bytes);
    write_frame_compressed_into(stream, &bytes, comp)
        .await
        .map(|_| ())
}

// ─── QUIC connection handler ──────────────────────────────────────────────────

/// Run a QUIC client session on pre-accepted I/O halves.
///
/// Called by `startup.rs` after `QuicListener` accepts a connection and splits
/// the control stream.  The `conn` is captured by `QuicAttacher` for per-pane
/// unidirectional streams.
pub async fn handle_with_io<R, W>(
    reader: R,
    writer: W,
    conn: Connection,
    app: Arc<ServerApp>,
    transport: kmux_protocol::TransportKind,
    conn_span: tracing::Span,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run_client_session(
        reader,
        writer,
        app,
        transport,
        |ctrl_tx, comp_out| QuicAttacher {
            conn: conn.clone(),
            comp_out,
            ctrl_tx,
            stall_timeout: PANE_STREAM_STALL_TIMEOUT,
        },
        conn_span.clone(),
    )
    .instrument(conn_span)
    .await;
}

#[cfg(test)]
mod classify_tests {
    use super::classify_error;
    use kmux_protocol::messages::ErrorCode;
    use kmux_pty::error::KmuxError;

    #[test]
    fn each_lookup_miss_keeps_its_own_code() {
        assert_eq!(
            classify_error(&KmuxError::SessionNotFound {
                name: "eagle".to_string()
            }),
            ErrorCode::SessionNotFound
        );
        assert_eq!(
            classify_error(&KmuxError::PaneNotFound {
                id: "eagle/0".to_string()
            }),
            ErrorCode::PaneNotFound,
            "a pane miss must not be reported as a missing session"
        );
        assert_eq!(
            classify_error(&KmuxError::SessionAlreadyExists {
                name: "eagle".to_string()
            }),
            ErrorCode::SessionAlreadyExists
        );
        assert_eq!(
            classify_error(&KmuxError::SessionLimit { max: 1000 }),
            ErrorCode::SessionLimitReached,
            "the limit is not a name clash"
        );
    }

    #[test]
    fn only_eperm_among_pty_errnos_means_the_input_is_locked() {
        assert_eq!(
            classify_error(&KmuxError::Pty(nix::Error::EPERM)),
            ErrorCode::InputLocked
        );
        assert_eq!(
            classify_error(&KmuxError::Pty(nix::Error::EINVAL)),
            ErrorCode::InternalError
        );
    }

    #[test]
    fn anything_unclassified_is_an_internal_error() {
        assert_eq!(classify_error(&KmuxError::Closed), ErrorCode::InternalError);
        assert_eq!(
            classify_error(&KmuxError::Timeout),
            ErrorCode::InternalError
        );
    }
}

#[cfg(test)]
mod pane_writer_tests {
    use tokio::io::AsyncReadExt;

    use super::*;

    fn frame() -> ServerMessage {
        ServerMessage::LogChunk {
            request_id: 1,
            data: vec![0; 4096],
        }
    }

    /// A client that stops reading a pane stream stalls the frame write, and
    /// after the write timeout the writer gives up with `WriteTimeout`
    /// instead of waiting forever (issue #207).
    #[tokio::test(start_paused = true)]
    async fn a_pane_stream_nobody_reads_times_out() {
        // The client half is kept but never read, so writes block.
        let (mut server, _client) = tokio::io::duplex(64);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();
        let started = tokio::time::Instant::now();

        let written = pane_uni_writer(
            &mut server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            PANE_STREAM_STALL_TIMEOUT,
        )
        .await;

        assert_eq!(written, Err(CloseReason::WriteTimeout));
        assert!(started.elapsed() >= PANE_STREAM_STALL_TIMEOUT);
    }

    /// A client that reads gets each queued frame, and the stream is finished
    /// once the pane's queue closes.
    #[tokio::test]
    async fn a_read_pane_stream_carries_the_frames_then_ends() {
        let (mut server, mut client) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();
        drop(tx);

        let written = pane_uni_writer(
            &mut server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            PANE_STREAM_STALL_TIMEOUT,
        )
        .await;

        assert_eq!(written, Ok(()));
        let mut wire = Vec::new();
        // Bounded: a writer that never finishes the stream must fail, not hang.
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut wire))
            .await
            .expect("the stream ends")
            .unwrap();
        let expected = encode_server(&frame()).unwrap();
        assert_eq!(wire.len(), 5 + expected.len(), "one uncompressed frame");
        assert!(wire.ends_with(&expected));
    }

    /// Whether `msg` is the `Lagged` a stall of `pane` sends.
    fn is_lagged(msg: &ServerMessage, pane: &str) -> bool {
        matches!(
            msg,
            ServerMessage::Lagged { pane_id, missed_count: 1 } if pane_id == pane
        )
    }

    /// Only a stall asks the client to resync the pane; a stream that ended
    /// or failed on its own does not.
    #[test]
    fn only_a_stalled_pane_stream_is_resynced() {
        let resync = stall_resync(Err(CloseReason::WriteTimeout), "eagle/0");
        assert!(is_lagged(&resync.expect("a resync"), "eagle/0"));
        assert!(stall_resync(Err(CloseReason::WriteFailed), "eagle/0").is_none());
        assert!(stall_resync(Ok(()), "eagle/0").is_none());
    }

    /// The stall timeout outlasts the QUIC idle timeout, so an outage the
    /// connection rides out never resets a pane stream.
    #[test]
    fn a_stall_outlasts_the_quic_idle_timeout() {
        assert!(PANE_STREAM_STALL_TIMEOUT > Duration::from_secs(kmux_sys::QUIC_IDLE_TIMEOUT_SECS));
        assert_eq!(
            PANE_STREAM_STALL_TIMEOUT,
            Duration::from_secs(330),
            "30 s past it"
        );
    }

    /// A stream the client has gone from fails the write at once.
    #[tokio::test]
    async fn a_pane_stream_whose_client_is_gone_fails() {
        let (mut server, client) = tokio::io::duplex(64);
        drop(client);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();

        let written = pane_uni_writer(
            &mut server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            PANE_STREAM_STALL_TIMEOUT,
        )
        .await;

        assert_eq!(written, Err(CloseReason::WriteFailed));
    }

    // ─── On a real QUIC connection ───────────────────────────────────────────
    //
    // A reset is a QUIC frame; `duplex` has no equivalent, so these run over
    // loopback on the real clock, with a short stall timeout.

    /// A bound on every loopback wait: loopback takes milliseconds, and a
    /// mutant that hangs one should fail fast.
    const WAIT: Duration = Duration::from_secs(5);

    /// The stall timeout the loopback tests use.
    const SHORT_STALL: Duration = Duration::from_millis(200);

    /// A connected QUIC client and server over loopback.
    async fn quic_pair() -> (quinn::Endpoint, quinn::Endpoint, Connection, Connection) {
        use kmux_sys::tls::{CertMaterial, TofuVerifier, build_server_config};

        let tls = build_server_config(CertMaterial::self_signed().unwrap()).unwrap();
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
        let server = quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();

        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TofuVerifier::accept_invalid(
                addr.to_string(),
                "quic",
            )))
            .with_no_client_auth();
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));

        let connecting = client.connect(addr, "localhost").unwrap();
        let accepting = async { server.accept().await.expect("an attempt").await };
        let (client_conn, server_conn) =
            tokio::time::timeout(WAIT, async { tokio::join!(connecting, accepting) })
                .await
                .expect("the handshake completes");
        (client, server, client_conn.unwrap(), server_conn.unwrap())
    }

    fn attacher(conn: Connection, ctrl_tx: OutboundTx) -> QuicAttacher {
        QuicAttacher {
            conn,
            comp_out: Arc::new(OutboundCompression::new(3, 1024)),
            ctrl_tx,
            stall_timeout: SHORT_STALL,
        }
    }

    /// A frame too big to compress well, so the flow-control window fills.
    fn big_frame() -> ServerMessage {
        let data = (0..64 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8);
        ServerMessage::LogChunk {
            request_id: 1,
            data: data.collect(),
        }
    }

    /// A pane stream the client stops reading is reset — that stream only:
    /// the connection stays open — and the client is sent a `Lagged` for the
    /// pane on the control channel, so it re-attaches and resyncs
    /// (issue #207).
    #[tokio::test]
    async fn a_stalled_pane_stream_is_reset_alone_and_the_pane_resynced() {
        let (_client, _server, client_conn, server_conn) = quic_pair().await;
        let (ctrl_tx, mut ctrl_rx) = crate::fixtures::make_outbound();
        let (tx, rx) = mpsc::channel(8);
        let feeding = tokio::spawn(async move {
            // More than any receive window: the stream stalls on its own.
            while tx.send(big_frame()).await.is_ok() {}
        });

        let handle = attacher(server_conn.clone(), ctrl_tx)
            .start_pane_stream("eagle/0".to_string(), AttachResult::Delta(vec![]), rx)
            .await
            .expect("the stream opens");

        let resync = tokio::time::timeout(WAIT, async {
            loop {
                if let Ok(msg) = ctrl_rx.try_recv() {
                    return msg;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("a resync is sent");
        assert!(is_lagged(&resync, "eagle/0"), "got {resync:?}");
        tokio::time::timeout(WAIT, async {
            while !handle.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the writer ends");
        tokio::time::timeout(WAIT, feeding)
            .await
            .expect("the pane queue closes with the writer")
            .unwrap();

        let mut uni = tokio::time::timeout(WAIT, client_conn.accept_uni())
            .await
            .expect("bounded")
            .expect("the pane stream");
        let read = tokio::time::timeout(WAIT, uni.read_to_end(usize::MAX))
            .await
            .expect("bounded");
        assert!(
            matches!(
                read,
                Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code)))
                    if code == PANE_STREAM_STALLED_CODE.into()
            ),
            "reset with the stall code, got {read:?}"
        );
        assert_eq!(
            server_conn.close_reason(),
            None,
            "the connection stays open"
        );
        assert_eq!(client_conn.close_reason(), None);
    }

    /// A pane stream the client reads is neither reset nor resynced.
    #[tokio::test]
    async fn a_read_pane_stream_is_not_reset() {
        let (_client, _server, client_conn, server_conn) = quic_pair().await;
        let (ctrl_tx, mut ctrl_rx) = crate::fixtures::make_outbound();
        let (tx, rx) = mpsc::channel(8);
        tx.send(big_frame()).await.unwrap();
        drop(tx);

        let handle = attacher(server_conn.clone(), ctrl_tx)
            .start_pane_stream("eagle/0".to_string(), AttachResult::Delta(vec![]), rx)
            .await
            .expect("the stream opens");
        let mut uni = tokio::time::timeout(WAIT, client_conn.accept_uni())
            .await
            .expect("bounded")
            .expect("the pane stream");
        let wire = tokio::time::timeout(WAIT, uni.read_to_end(usize::MAX))
            .await
            .expect("bounded")
            .expect("finished, not reset");
        let expected = encode_server(&big_frame()).unwrap();
        assert!(wire.ends_with(&expected));
        tokio::time::timeout(WAIT, async {
            while !handle.is_finished() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the writer ends");
        assert!(ctrl_rx.try_recv().is_err(), "no resync");
    }

    /// A QUIC connection is served by a client session: a frame that is not
    /// a client message is answered with an `InvalidMessage` error.
    #[tokio::test]
    async fn handle_with_io_serves_the_connection() {
        use kmux_protocol::messages::ErrorCode;
        use kmux_protocol::{decode_server, read_frame, write_frame};

        let (_client, _server, _client_conn, server_conn) = quic_pair().await;
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        let session = tokio::spawn(handle_with_io(
            server_read,
            server_write,
            server_conn,
            Arc::new(crate::fixtures::fixture_app()),
            kmux_protocol::TransportKind::Quic,
            tracing::Span::none(),
        ));

        write_frame(&mut client_write, b"not a message")
            .await
            .unwrap();
        let frame = tokio::time::timeout(WAIT, read_frame(&mut client_read))
            .await
            .expect("answered in time")
            .unwrap()
            .expect("a reply");
        assert!(matches!(
            decode_server(&frame).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ));
        session.abort();
    }
}
