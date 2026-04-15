use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kmux_protocol::messages::{ErrorCode, ServerMessage, epoch_millis};
use kmux_protocol::{decode_client, encode_server, read_frame, write_frame};
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tracing::{debug, info, warn};

use crate::app::{AttachResult, ServerApp};
use crate::client_handler::{PaneAttacher, SharedClientState, handle_message, pty_event_to_msg};

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
                tcp_pane_forwarder(result, pane_id, &mut client_rx, ctrl_tx).await;
            })
            .abort_handle();
            Ok(handle)
        }
    }
}

// ─── TCP pane forwarder ───────────────────────────────────────────────────────

/// Forward initial replay data + live diffs from a pane attach into ctrl_tx.
///
/// This is the TCP equivalent of `pane_uni_writer` in connection.rs.
/// Instead of a dedicated uni-stream, all messages are interleaved on the
/// shared TCP control stream (ctrl_tx). This works because all `ServerMessage`
/// variants carry `pane_id` for client-side demultiplexing.
async fn tcp_pane_forwarder(
    attach_result: AttachResult,
    pane_id: String,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
    ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    match attach_result {
        AttachResult::FullSnapshot(snapshot, seqno) => {
            let _ = ctrl_tx.send(ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            });
        }
        AttachResult::Delta(diffs) => {
            for (seqno, diff) in diffs {
                let _ = ctrl_tx.send(ServerMessage::TerminalUpdate {
                    pane_id: pane_id.clone(),
                    diff,
                    seqno,
                    sent_at_ms: epoch_millis(),
                });
            }
        }
        AttachResult::SyncReset(snapshot, seqno) => {
            let _ = ctrl_tx.send(ServerMessage::SyncReset {
                pane_id: pane_id.clone(),
            });
            let _ = ctrl_tx.send(ServerMessage::TerminalSnapshot {
                pane_id: pane_id.clone(),
                snapshot,
                seqno,
                sent_at_ms: epoch_millis(),
            });
        }
    }

    while let Some(msg) = client_rx.recv().await {
        if ctrl_tx.send(msg).is_err() {
            break;
        }
    }
}

// ─── TCP connection handler ───────────────────────────────────────────────────

/// Handle a single TCP client connection.
async fn handle_tcp(stream: TcpStream, app: Arc<ServerApp>) {
    let (read_half, write_half): (ReadHalf<TcpStream>, WriteHalf<TcpStream>) =
        tokio::io::split(stream);

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

    // Writer task: drain ctrl_rx and write frames to the TCP stream.
    let writer_task = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(msg) = ctrl_rx.recv().await {
            match encode_server(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut write_half, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("TCP encode error: {e}"),
            }
        }
        let _ = write_half.shutdown().await;
    });

    let mut event_rx = app.subscribe_events();
    let event_tx = ctrl_tx.clone();
    let event_task = tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let msg = pty_event_to_msg(event);
            let _ = event_tx.send(ServerMessage::Event { event: msg });
        }
    });

    let ping_tx = ctrl_tx.clone();
    let ping_task = tokio::spawn(async move {
        let mut seq = 0u64;
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if ping_tx.send(ServerMessage::Ping { seq }).is_err() {
                break;
            }
            seq += 1;
        }
    });

    let attacher = TcpAttacher {
        ctrl_tx: ctrl_tx.clone(),
    };
    let mut state = SharedClientState::new(app.clone(), ctrl_tx, "TCP ");
    let mut read_half = read_half;

    loop {
        match read_frame(&mut read_half).await {
            Ok(Some(data)) => match decode_client(&data) {
                Ok(client_msg) => {
                    if !handle_message(&mut state, client_msg, &attacher).await {
                        break;
                    }
                }
                Err(e) => {
                    warn!("TCP decode error: {e}");
                    state.error(None, ErrorCode::InvalidMessage, e.to_string());
                }
            },
            Ok(None) => {
                debug!("TCP control stream closed");
                break;
            }
            Err(e) => {
                warn!("TCP read error: {e}");
                break;
            }
        }
    }

    event_task.abort();
    ping_task.abort();

    if let Some(client_id) = state.client_id {
        app.detach_client_all(client_id).await;
    }
    if let Some(conn_id) = state.connection_id {
        app.unregister_client(conn_id).await;
    }

    drop(state);
    writer_task.abort();
    info!("TCP connection closed");
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
