use std::net::SocketAddr;
use std::sync::Arc;

use kmux_protocol::messages::ServerMessage;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{info, warn};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, build_attach_replay, run_client_session};

/// Bind a TCP listener and accept connections in a loop.
///
/// Each accepted connection is handled in its own task using the same
/// `ClientMessage` / `ServerMessage` protocol as the QUIC transport, but over a
/// single TCP byte stream with length-prefixed postcard frames.
pub async fn serve_tcp(addr: SocketAddr, app: Arc<ServerApp>) -> anyhow::Result<u16> {
    let listener = TcpListener::bind(addr).await?;
    let actual_port = listener.local_addr()?.port();
    info!("TCP transport listening on {}", listener.local_addr()?);

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    info!("TCP connection from {peer}");
                    let app = Arc::clone(&app);
                    tokio::spawn(async move {
                        handle_tcp(stream, app).await;
                    });
                }
                Err(e) => {
                    warn!("TCP accept error: {e}");
                }
            }
        }
    });

    Ok(actual_port)
}

// ─── TCP-specific PaneAttacher ────────────────────────────────────────────────

/// Forwards pane diffs from `client_rx` into the shared TCP control channel
/// (`ctrl_tx`).  All messages are interleaved on the single TCP byte stream;
/// the client demultiplexes them by the `pane_id` field carried in each
/// `ServerMessage` variant.
struct TcpAttacher {
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
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

// ─── TCP connection handler ───────────────────────────────────────────────────

/// Handle a single TCP client connection.
async fn handle_tcp(stream: TcpStream, app: Arc<ServerApp>) {
    let (read_half, write_half) = tokio::io::split(stream);
    run_client_session(
        read_half,
        write_half,
        app,
        |ctrl_tx| TcpAttacher { ctrl_tx },
        "TCP ",
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// Verify that the TCP listener binds successfully on port 0 (random).
    #[tokio::test]
    async fn tcp_listener_binds_random_port() {
        let app = Arc::new(ServerApp::new("test-token".to_string()));
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let port = serve_tcp(addr, app).await.expect("should bind");
        assert!(port > 0, "expected a non-zero port");
    }
}
