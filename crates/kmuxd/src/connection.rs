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
    FRAME_WRITE_TIMEOUT, MAX_WRITE_BATCH, OutboundCompression, PaneAttacher, build_attach_replay,
    run_client_session, within,
};
use crate::outbound::CloseReason;

pub fn classify_error(e: &kmux_pty::error::KmuxError) -> ErrorCode {
    match e {
        kmux_pty::error::KmuxError::SessionNotFound { .. } => ErrorCode::SessionNotFound,
        kmux_pty::error::KmuxError::PaneNotFound { .. } => ErrorCode::PaneNotFound,
        kmux_pty::error::KmuxError::SessionAlreadyExists { .. } => ErrorCode::SessionAlreadyExists,
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
        async move {
            let uni_stream = conn
                .open_uni()
                .await
                .map_err(|e| format!("failed to open uni stream: {e}"))?;
            let handle = tokio::spawn(async move {
                let written = pane_uni_writer(
                    uni_stream,
                    result,
                    &pane_id,
                    &mut client_rx,
                    &comp_out,
                    FRAME_WRITE_TIMEOUT,
                )
                .await;
                if closes_connection(written) {
                    conn.close(PANE_WRITE_TIMEOUT_CODE.into(), b"pane stream write timeout");
                }
            })
            .abort_handle();
            Ok(handle)
        }
    }
}

// ─── QUIC pane stream writer ──────────────────────────────────────────────────

/// QUIC application error code a connection is closed with when one of its
/// pane streams stops taking frames.
const PANE_WRITE_TIMEOUT_CODE: u32 = 1;

/// Whether a pane stream's end closes the whole QUIC connection. A client that
/// stopped reading one pane's stream is treated like one that stopped reading
/// the control stream: the connection closes, and the client reconnects and
/// resyncs (issue #207). A stream that ended or failed on its own does not.
fn closes_connection(written: Result<(), CloseReason>) -> bool {
    written == Err(CloseReason::WriteTimeout)
}

/// Write initial replay data + live diffs on a server-initiated unidirectional
/// stream, until the pane's queue closes or a write fails.
///
/// Every frame write and every flush must finish within `write_timeout`
/// (issue #207), as on the TCP/UDS writer: a client that stops reading a pane
/// stream fills its QUIC flow-control window, and the write would otherwise
/// wait forever, pinning this task and the pane's queue. Such a stall is
/// [`CloseReason::WriteTimeout`]; a failed write is
/// [`CloseReason::WriteFailed`].
async fn pane_uni_writer<W: AsyncWrite + Unpin>(
    mut uni: W,
    attach_result: AttachResult,
    pane_id: &str,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
    comp_out: &OutboundCompression,
    write_timeout: Duration,
) -> Result<(), CloseReason> {
    // Replay frames are written as whole frames then flushed once.
    for msg in build_attach_replay(attach_result, pane_id) {
        within(
            write_timeout,
            write_frame(&mut uni, &msg, comp_out.compressor()),
        )
        .await?;
    }
    within(write_timeout, kmux_protocol::flush(&mut uni)).await?;

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
            within(
                write_timeout,
                write_frame(&mut uni, &msg, comp_out.compressor()),
            )
            .await?;
            let write_us = write_start.elapsed().as_micros();
            if write_us > 1000 {
                debug!(pane_id, write_us, "slow uni stream write");
            }
        }
        within(write_timeout, kmux_protocol::flush(&mut uni)).await?;
    }

    // Finishing the stream (a QUIC `FIN`) must not wait forever either.
    let _ = tokio::time::timeout(write_timeout, uni.shutdown()).await;
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
        |_ctrl_tx, comp_out| QuicAttacher {
            conn: conn.clone(),
            comp_out,
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
        let (server, _client) = tokio::io::duplex(64);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();
        let started = tokio::time::Instant::now();

        let written = pane_uni_writer(
            server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            FRAME_WRITE_TIMEOUT,
        )
        .await;

        assert_eq!(written, Err(CloseReason::WriteTimeout));
        assert!(started.elapsed() >= FRAME_WRITE_TIMEOUT);
    }

    /// A client that reads gets each queued frame, and the stream is finished
    /// once the pane's queue closes.
    #[tokio::test]
    async fn a_read_pane_stream_carries_the_frames_then_ends() {
        let (server, mut client) = tokio::io::duplex(64 * 1024);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();
        drop(tx);

        let written = pane_uni_writer(
            server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            FRAME_WRITE_TIMEOUT,
        )
        .await;

        assert_eq!(written, Ok(()));
        let mut wire = Vec::new();
        client.read_to_end(&mut wire).await.unwrap();
        let expected = encode_server(&frame()).unwrap();
        assert_eq!(wire.len(), 5 + expected.len(), "one uncompressed frame");
        assert!(wire.ends_with(&expected));
    }

    #[test]
    fn only_a_stalled_pane_stream_closes_the_connection() {
        assert!(closes_connection(Err(CloseReason::WriteTimeout)));
        assert!(!closes_connection(Err(CloseReason::WriteFailed)));
        assert!(!closes_connection(Ok(())));
    }

    /// A stream the client has gone from fails the write at once.
    #[tokio::test]
    async fn a_pane_stream_whose_client_is_gone_fails() {
        let (server, client) = tokio::io::duplex(64);
        drop(client);
        let (tx, mut rx) = mpsc::channel(4);
        tx.send(frame()).await.unwrap();

        let written = pane_uni_writer(
            server,
            AttachResult::Delta(vec![]),
            "eagle/0",
            &mut rx,
            &OutboundCompression::new(3, 1024),
            FRAME_WRITE_TIMEOUT,
        )
        .await;

        assert_eq!(written, Err(CloseReason::WriteFailed));
    }
}
