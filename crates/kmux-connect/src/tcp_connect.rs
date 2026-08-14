use std::path::Path;
#[cfg(feature = "remote")]
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ConnectionId, ServerMessage};
#[cfg(feature = "remote")]
use kmux_protocol::tls::{TofuStore, TofuVerifier};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::io::AsyncWrite;
#[cfg(feature = "remote")]
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::connect::ConnectResult;

/// Encode and write the initial `Auth` frame on a freshly-opened control stream.
///
/// Centralises the auth handshake payload — token + supported protocol range +
/// named protocol/application capabilities + `connection_id` + this process's identity claim
/// (public key + hostname/username, issue #146) — so every transport (UDS / TCP
/// / TCP+TLS / QUIC) sends a byte-identical frame and a new `Auth` field is wired
/// in exactly one place. The daemon replies with an `AuthChallenge` the caller
/// answers via [`answer_auth_challenge`]. Returns a human-readable error; the
/// caller wraps it in [`ConnectResult::Failed`].
pub(crate) async fn send_auth_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    token: String,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
) -> Result<(), String> {
    let (public_key, hostname, username) = local_identity_claim();
    let auth_bytes = encode_client(&ClientMessage::Auth {
        token,
        protocol_range: kmux_protocol::messages::PROTOCOL_RANGE,
        protocol_capabilities: kmux_protocol::messages::protocol_capabilities(),
        capabilities,
        connection_id,
        public_key,
        hostname,
        username,
        // Build identity, so the daemon can attribute the connection and detect a
        // client whose build differs from its own (issue: build skew). The kind
        // is the process-wide frontend; sha/profile come from this binary's build.
        client_kind: crate::frontend_kind(),
        client_git_sha: kmux_protocol::buildinfo::git_sha().to_string(),
        client_git_dirty: kmux_protocol::buildinfo::git_dirty(),
        client_build_profile: kmux_protocol::buildinfo::build_profile().to_string(),
    })
    .map_err(|e| format!("auth encode failed: {e}"))?;
    write_frame(writer, &auth_bytes)
        .await
        .map_err(|e| format!("auth write failed: {e}"))
}

/// This process's identity claim for the `Auth` handshake (issue #146): the
/// local Ed25519 public key plus friendly hostname/username labels. A failure to
/// load the key yields an empty key (the daemon then rejects the handshake).
fn local_identity_claim() -> (Vec<u8>, String, String) {
    let public_key = match kmux_protocol::identity::Identity::load_or_create() {
        Ok(id) => id.public_key_bytes().to_vec(),
        Err(e) => {
            warn!("failed to load identity key: {e}");
            Vec::new()
        }
    };
    (
        public_key,
        kmux_protocol::identity::local_hostname(),
        kmux_protocol::identity::local_username(),
    )
}

/// Sign a server challenge `nonce` with the local identity and send the
/// resulting [`ClientMessage::AuthProof`] upstream (issue #146). Returns `false`
/// if the identity can't be loaded or the sink is closed.
pub fn answer_auth_challenge(
    client_tx: &mpsc::UnboundedSender<ClientMessage>,
    nonce: &[u8],
) -> bool {
    let identity = match kmux_protocol::identity::Identity::load_or_create() {
        Ok(id) => id,
        Err(e) => {
            warn!("failed to load identity to answer challenge: {e}");
            return false;
        }
    };
    client_tx
        .send(ClientMessage::AuthProof {
            signature: identity.sign(nonce),
        })
        .is_ok()
}

/// Establish a TCP connection to `host:port` and authenticate with `token`.
///
/// All messages (control + pane diffs) flow over a single TCP stream using
/// the same length-prefixed MessagePack frame format as the QUIC transport.
/// The server interleaves `ServerMessage` values on the stream; the client
/// dispatches them by message type (pane_id fields handle routing).
///
/// Pass `connection_id = Some(id)` to resume an existing session after a
/// transport switch (e.g. QUIC → TCP fallback).
#[cfg(feature = "remote")]
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
    if let Err(e) = send_auth_frame(&mut write_half, token, capabilities, connection_id).await {
        return ConnectResult::Failed(e);
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

/// Establish a TLS-over-TCP connection to `host:port` and authenticate with `token`.
///
/// Certificate verification uses TOFU (`known_hosts.toml`).  The `tofu_key`
/// parameter lets callers separate the connection address from the TOFU identity:
/// for SSH-tunnel connections pass `"remote_host:remote_port"` so the pin is
/// keyed to the actual server, not the ephemeral loopback port.
#[cfg(feature = "remote")]
#[allow(clippy::too_many_arguments)]
pub async fn connect_tcp_tls(
    host: String,
    port: u16,
    tofu_key: String,
    token: String,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
    accept_invalid: bool,
) -> ConnectResult {
    use rustls::pki_types::ServerName;
    use tokio_rustls::TlsConnector;

    let verifier = match build_tofu_verifier(tofu_key, "tcp+tls", accept_invalid) {
        Ok(verifier) => verifier,
        Err(e) => return ConnectResult::Failed(e),
    };
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client_config));

    let server_name = match ServerName::try_from(host.as_str()) {
        Ok(n) => n.to_owned(),
        Err(e) => return ConnectResult::Failed(format!("invalid server name '{host}': {e}")),
    };

    let stream = match TcpStream::connect(format!("{host}:{port}")).await {
        Ok(s) => s,
        Err(e) => {
            return ConnectResult::Failed(format!("TCP connect to {host}:{port} failed: {e}"));
        }
    };
    if let Err(e) = set_tcp_keepalive(&stream) {
        warn!("Failed to set TCP keepalive: {e}");
    }

    let tls_stream = match connector.connect(server_name, stream).await {
        Ok(s) => s,
        Err(e) => {
            return ConnectResult::Failed(format!("TLS handshake with {host}:{port} failed: {e}"));
        }
    };

    let (mut read_half, mut write_half) = tokio::io::split(tls_stream);

    if let Err(e) = send_auth_frame(&mut write_half, token, capabilities, connection_id).await {
        return ConnectResult::Failed(e);
    }

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match encode_client(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut write_half, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("TCP+TLS encode error: {e}"),
            }
        }
        debug!("TCP+TLS writer task exited");
    });

    tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(Some(data)) => match decode_server(&data) {
                    Ok(msg) => {
                        if server_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("TCP+TLS decode error: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!("TCP+TLS read error: {e}");
                    break;
                }
            }
        }
        writer_handle.abort();
        debug!("TCP+TLS reader task exited");
    });

    ConnectResult::Connected(client_tx)
}

/// Establish a Unix domain socket connection to `socket_path` and authenticate.
///
/// No TLS is used — UDS connections are local and the OS enforces ownership
/// via socket permissions (0600) and optionally peer-credential checks.
pub async fn connect_uds(
    socket_path: impl AsRef<Path>,
    token: String,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
) -> ConnectResult {
    let socket_path = socket_path.as_ref();

    let stream = match tokio::net::UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            return ConnectResult::Failed(format!(
                "UDS connect to {} failed: {e}",
                socket_path.display()
            ));
        }
    };

    let (mut read_half, mut write_half) = tokio::io::split(stream);

    if let Err(e) = send_auth_frame(&mut write_half, token, capabilities, connection_id).await {
        return ConnectResult::Failed(e);
    }

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match encode_client(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut write_half, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("UDS encode error: {e}"),
            }
        }
        debug!("UDS writer task exited");
    });

    tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(Some(data)) => match decode_server(&data) {
                    Ok(msg) => {
                        if server_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("UDS decode error: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!("UDS read error: {e}");
                    break;
                }
            }
        }
        writer_handle.abort();
        debug!("UDS reader task exited");
    });

    ConnectResult::Connected(client_tx)
}

/// Build a certificate verifier, loading the persistent pin store only when
/// certificate checks are enabled.
#[cfg(feature = "remote")]
pub(crate) fn build_tofu_verifier(
    key: String,
    transport: &'static str,
    accept_invalid: bool,
) -> Result<TofuVerifier, String> {
    if accept_invalid {
        return Ok(TofuVerifier::accept_invalid(key, transport));
    }
    let path = kmux_protocol::dirs::known_hosts_path()
        .map_err(|e| format!("cannot determine known_hosts path: {e}"))?;
    TofuStore::load(path.clone())
        .map(|store| Arc::new(Mutex::new(store)))
        .map_err(|e| format!("failed to load known_hosts {}: {e}", path.display()))
        .map(|store| TofuVerifier::new(key, transport, store))
}

#[cfg(feature = "remote")]
fn set_tcp_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    use nix::sys::socket::{setsockopt, sockopt};
    use std::os::unix::io::AsRawFd;

    let fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(stream.as_raw_fd()) };
    setsockopt(&fd, sockopt::KeepAlive, &true)
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))
}
