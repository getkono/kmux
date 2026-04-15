use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ConnectionId, ServerMessage};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::connect::ConnectResult;

/// Establish a TCP connection to `host:port` and authenticate with `token`.
///
/// All messages (control + pane diffs) flow over a single TCP stream using
/// the same length-prefixed postcard frame format as the QUIC transport.
/// The server interleaves `ServerMessage` values on the stream; the client
/// dispatches them by message type (pane_id fields handle routing).
///
/// Pass `connection_id = Some(id)` to resume an existing session after a
/// transport switch (e.g. QUIC → TCP fallback).
pub async fn connect_tcp(
    host: String,
    port: u16,
    token: String,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
) -> ConnectResult {
    let stream = match TcpStream::connect(format!("{host}:{port}")).await {
        Ok(s) => s,
        Err(e) => return ConnectResult::Failed(format!("TCP connect failed: {e}")),
    };

    // Enable TCP keepalive so the OS detects dead connections.
    if let Err(e) = set_tcp_keepalive(&stream) {
        warn!("Failed to set TCP keepalive: {e}");
    }

    let (mut read_half, mut write_half) = stream.into_split();

    // Authenticate immediately.
    let auth_bytes = match encode_client(&ClientMessage::Auth {
        token,
        protocol_version: kmux_protocol::messages::PROTOCOL_VERSION,
        capabilities,
        connection_id,
    }) {
        Ok(bytes) => bytes,
        Err(e) => return ConnectResult::Failed(format!("TCP auth encode failed: {e}")),
    };
    if let Err(e) = write_frame(&mut write_half, &auth_bytes).await {
        return ConnectResult::Failed(format!("TCP auth write failed: {e}"));
    }

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

    // Writer task: drain client_rx and write frames to the TCP stream.
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match encode_client(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut write_half, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("TCP encode error: {e}"),
            }
        }
        debug!("TCP writer task exited");
    });

    // Reader task: read frames from the TCP stream and forward to server_tx.
    tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(Some(data)) => match decode_server(&data) {
                    Ok(msg) => {
                        if server_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("TCP decode error: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!("TCP read error: {e}");
                    break;
                }
            }
        }
        writer_handle.abort();
        debug!("TCP reader task exited");
    });

    ConnectResult::Connected(client_tx)
}

fn set_tcp_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};
    use std::os::unix::io::AsRawFd;

    let fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(stream.as_raw_fd()) };
    setsockopt(&fd, sockopt::KeepAlive, &true)
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}
