use std::sync::Arc;
use std::time::Instant;

use kmux_protocol::messages::{ErrorCode, ServerMessage};
use kmux_protocol::{encode_server, write_frame};
use quinn::Connection;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{Instrument, debug};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, build_attach_replay, run_client_session};

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
    for msg in build_attach_replay(attach_result, &pane_id) {
        if send_frame(&mut uni, &msg).await.is_err() {
            return;
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
    conn_span: tracing::Span,
) where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    run_client_session(
        reader,
        writer,
        app,
        |_| QuicAttacher { conn: conn.clone() },
        conn_span.clone(),
    )
    .instrument(conn_span)
    .await;
}
