use std::sync::Arc;

use kmux_protocol::messages::ServerMessage;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::Instrument;

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, run_client_session};
use crate::outbound::{OutboundTx, forward_pane_stream};

// ─── TCP-specific PaneAttacher ────────────────────────────────────────────────

/// Forwards pane diffs from `client_rx` into the connection's outbound queue
/// (`ctrl_tx`) as pane data. All messages are interleaved on the single TCP
/// byte stream; the client demultiplexes them by the `pane_id` field carried
/// in each `ServerMessage` variant. A congested queue lags the pane stream and
/// resyncs it from `app` (see [`forward_pane_stream`]).
pub(crate) struct TcpAttacher {
    pub ctrl_tx: OutboundTx,
    pub app: Arc<ServerApp>,
}

impl PaneAttacher for TcpAttacher {
    fn start_pane_stream(
        &self,
        pane_id: String,
        result: AttachResult,
        client_rx: mpsc::Receiver<ServerMessage>,
    ) -> impl Future<Output = Result<AbortHandle, String>> + Send {
        let ctrl_tx = self.ctrl_tx.clone();
        let app = Arc::clone(&self.app);
        async move {
            // Network impairment shim (issue #72): delay live pane-data frames
            // before they reach the shared TCP writer. Applied here — not in
            // the writer loop — so Ping/control are never blocked.
            let client_rx = match crate::impair::config() {
                Some(cfg) => impaired(cfg, &pane_id, client_rx),
                None => client_rx,
            };
            let resync_pane = pane_id.clone();
            let handle = tokio::spawn(forward_pane_stream(
                pane_id,
                result,
                client_rx,
                ctrl_tx,
                move || {
                    let app = Arc::clone(&app);
                    let pane_id = resync_pane.clone();
                    async move { app.resync_snapshot(&pane_id).await }
                },
            ))
            .abort_handle();
            Ok(handle)
        }
    }
}

/// Re-queue `rx` through a task that applies the impairment shim's per-frame
/// delay. Only built when the impairment knobs are set.
fn impaired(
    cfg: &'static crate::impair::ImpairConfig,
    pane_id: &str,
    mut rx: mpsc::Receiver<ServerMessage>,
) -> mpsc::Receiver<ServerMessage> {
    let (tx, delayed) = mpsc::channel(crate::client_handler::CLIENT_CHANNEL_CAPACITY);
    let mut rng = cfg.rng_for(crate::impair::pane_salt(pane_id));
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            crate::impair::maybe_delay(cfg, msg.category(), &mut rng).await;
            if tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    delayed
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
    let attacher_app = Arc::clone(&app);
    run_client_session(
        reader,
        writer,
        app,
        transport,
        // The TCP/UDS attacher funnels pane diffs through `ctrl_tx`, so the
        // shared writer task compresses them; the per-connection policy is
        // unused here.
        move |ctrl_tx, _comp_out| TcpAttacher {
            ctrl_tx,
            app: attacher_app,
        },
        conn_span.clone(),
    )
    .instrument(conn_span)
    .await;
}

#[cfg(test)]
mod tests {
    use kmux_protocol::messages::ErrorCode;
    use kmux_protocol::{decode_server, read_frame, write_frame};

    use super::*;
    use crate::fixtures::fixture_app;

    /// A stream connection is served by a client session: a frame that is
    /// not a client message is answered with an `InvalidMessage` error.
    #[tokio::test]
    async fn handle_tcp_io_serves_the_connection() {
        let (server, client) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let (mut client_read, mut client_write) = tokio::io::split(client);
        tokio::spawn(handle_tcp_io(
            server_read,
            server_write,
            Arc::new(fixture_app()),
            kmux_protocol::TransportKind::Uds,
            tracing::Span::none(),
        ));

        write_frame(&mut client_write, b"not a message")
            .await
            .unwrap();
        let frame = read_frame(&mut client_read)
            .await
            .unwrap()
            .expect("a reply");
        assert!(matches!(
            decode_server(&frame).unwrap(),
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ));
    }
}
