use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{ErrorCode, ServerMessage};
use kmux_protocol::{Compressor, encode_server, write_frame_compressed};
use quinn::Connection;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{Instrument, debug};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{
    OutboundCompression, PaneAttacher, build_attach_replay, run_client_session,
};

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
    /// Shared outbound compression policy for this connection's pane streams.
    comp_out: Arc<OutboundCompression>,
}

impl PaneAttacher for QuicAttacher {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        mut client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl std::future::Future<Output = Result<AbortHandle, String>> + Send {
        let conn = self.conn.clone();
        let comp_out = Arc::clone(&self.comp_out);
        async move {
            let uni_stream = conn
                .open_uni()
                .await
                .map_err(|e| format!("failed to open uni stream: {e}"))?;
            let handle = tokio::spawn(async move {
                pane_uni_writer(uni_stream, result, pane_id, &mut client_rx, &comp_out).await;
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
    comp_out: &OutboundCompression,
) {
    for msg in build_attach_replay(attach_result, &pane_id) {
        if send_frame(&mut uni, &msg, comp_out.compressor())
            .await
            .is_err()
        {
            return;
        }
    }

    // Network impairment shim (issue #72): per-pane jitter on live pane-data
    // frames only. `None` (no env knobs) skips the delay entirely.
    let impair = crate::impair::config();
    let mut rng = impair.map(|c| c.rng_for(crate::impair::pane_salt(&pane_id)));

    while let Some(msg) = client_rx.recv().await {
        if let (Some(cfg), Some(rng)) = (impair, rng.as_mut()) {
            crate::impair::maybe_delay(cfg, msg.category(), rng).await;
        }
        let write_start = Instant::now();
        if send_frame(&mut uni, &msg, comp_out.compressor())
            .await
            .is_err()
        {
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
    comp: Compressor,
) -> Result<(), kmux_protocol::ProtocolError> {
    let bytes = encode_server(msg)?;
    if bytes.len() > 4096 {
        debug!(frame_bytes = bytes.len(), "large frame");
    }
    crate::capture::record(msg.category(), &bytes);
    write_frame_compressed(stream, &bytes, comp)
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
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
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
