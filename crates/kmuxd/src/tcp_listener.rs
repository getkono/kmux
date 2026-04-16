use std::sync::Arc;

use kmux_protocol::messages::ServerMessage;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::Instrument;

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, build_attach_replay, run_client_session};

// ─── TCP-specific PaneAttacher ────────────────────────────────────────────────

/// Forwards pane diffs from `client_rx` into the shared TCP control channel
/// (`ctrl_tx`).  All messages are interleaved on the single TCP byte stream;
/// the client demultiplexes them by the `pane_id` field carried in each
/// `ServerMessage` variant.
pub(crate) struct TcpAttacher {
    pub ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
}

impl PaneAttacher for TcpAttacher {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        mut client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl std::future::Future<Output = Result<AbortHandle, String>> + Send {
        let ctrl_tx = self.ctrl_tx.clone();
        async move {
            let handle = tokio::spawn(async move {
                for msg in build_attach_replay(result, &pane_id) {
                    if ctrl_tx.send(msg).is_err() {
                        return;
                    }
                }
                while let Some(msg) = client_rx.recv().await {
                    if ctrl_tx.send(msg).is_err() {
                        break;
                    }
                }
            })
            .abort_handle();
            Ok(handle)
        }
    }
}

// ─── TCP session handler ──────────────────────────────────────────────────────

/// Run a TCP/UDS client session on pre-split I/O halves.
///
/// Called by `startup.rs` after `PlainTcpListener` (or `TlsTcpListener` in
/// Phase 4) accepts a connection and wraps the stream in boxed I/O.
pub async fn handle_tcp_io<R, W>(
    reader: R,
    writer: W,
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
        |ctrl_tx| TcpAttacher { ctrl_tx },
        conn_span.clone(),
    )
    .instrument(conn_span)
    .await;
}
