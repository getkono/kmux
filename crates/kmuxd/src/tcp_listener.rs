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
                // Network impairment shim (issue #72): delay live pane-data
                // frames before they reach the shared TCP writer. Applied here —
                // not in the writer loop — so Ping/control are never blocked.
                let impair = crate::impair::config();
                let mut rng = impair.map(|c| c.rng_for(crate::impair::pane_salt(&pane_id)));
                while let Some(msg) = client_rx.recv().await {
                    if let (Some(cfg), Some(rng)) = (impair, rng.as_mut()) {
                        crate::impair::maybe_delay(cfg, msg.category(), rng).await;
                    }
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
        // The TCP/UDS attacher funnels pane diffs through `ctrl_tx`, so the
        // shared writer task compresses them; the per-connection policy is
        // unused here.
        |ctrl_tx, _comp_out| TcpAttacher { ctrl_tx },
        conn_span.clone(),
    )
    .instrument(conn_span)
    .await;
}
