#[cfg(feature = "remote")]
use std::net::ToSocketAddrs;
#[cfg(feature = "remote")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "remote")]
use std::time::Duration;

use kmux_protocol::messages::ClientMessage;
#[cfg(feature = "remote")]
use kmux_protocol::messages::{ClientCapabilities, ConnectionId, ServerMessage};
#[cfg(feature = "remote")]
use kmux_protocol::tls::{TofuStore, TofuVerifier};
#[cfg(feature = "remote")]
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
#[cfg(feature = "remote")]
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
#[cfg(feature = "remote")]
use tracing::{debug, warn};

/// Maximum number of concurrent server-initiated uni streams (one per attached session).
#[cfg(feature = "remote")]
const MAX_UNI_STREAMS: usize = 64;

/// Outcome of a connection attempt.
///
/// Always available (the local UDS path returns it too); only the QUIC
/// [`connect`] producer below is gated behind the `remote` feature.
pub enum ConnectResult {
    /// Connected successfully; returns a sender for outbound messages.
    Connected(mpsc::UnboundedSender<ClientMessage>),
    /// Connection failed with an error description.
    Failed(String),
}

/// Establish a QUIC connection to `host:port` and authenticate with `token`.
///
/// Uses a multi-stream model:
/// - Opens one bidirectional stream as the control channel
/// - Accepts server-initiated unidirectional streams for per-session diffs
///
/// The `server_tx` channel sends `ServerMessage` values back to the caller.
#[cfg(feature = "remote")]
pub async fn connect(
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
) -> ConnectResult {
    let addr = match format!("{host}:{port}")
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        Some(a) => a,
        None => return ConnectResult::Failed(format!("cannot resolve {host}:{port}")),
    };

    let client_config = build_quinn_client_config(&host, port, accept_invalid_certs);

    let mut endpoint = match quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()) {
        Ok(ep) => ep,
        Err(e) => return ConnectResult::Failed(format!("QUIC endpoint error: {e}")),
    };
    endpoint.set_default_client_config(client_config);

    let conn = match endpoint.connect(addr, &host) {
        Ok(connecting) => match connecting.await {
            Ok(c) => c,
            Err(e) => return ConnectResult::Failed(format!("QUIC connect failed: {e}")),
        },
        Err(e) => return ConnectResult::Failed(format!("QUIC connect error: {e}")),
    };

    // Open the control stream (first bidirectional stream)
    let (mut ctrl_send, mut ctrl_recv) = match conn.open_bi().await {
        Ok(streams) => streams,
        Err(e) => return ConnectResult::Failed(format!("control stream error: {e}")),
    };

    // Authenticate immediately
    if let Err(e) =
        crate::tcp_connect::send_auth_frame(&mut ctrl_send, token, capabilities, connection_id)
            .await
    {
        return ConnectResult::Failed(e);
    }

    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

    // Writer task: drain client_rx and send frames on the control stream
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match encode_client(&msg) {
                Ok(bytes) => {
                    if write_frame(&mut ctrl_send, &bytes).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("encode error: {e}"),
            }
        }
        let _ = ctrl_send.finish();
        debug!("Writer task exited");
    });

    // Reader task: decode incoming frames from the control stream
    let ctrl_server_tx = server_tx.clone();
    tokio::spawn(async move {
        loop {
            match read_frame(&mut ctrl_recv).await {
                Ok(Some(data)) => match decode_server(&data) {
                    Ok(msg) => {
                        if ctrl_server_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("decode error: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!("control stream read error: {e}");
                    break;
                }
            }
        }
        writer_handle.abort();
        debug!("Control reader task exited");
    });

    // Uni stream acceptor: accept server-initiated uni streams (per-session diffs).
    // A semaphore limits concurrent stream handlers to prevent unbounded task growth.
    let uni_server_tx = server_tx;
    let sem = Arc::new(Semaphore::new(MAX_UNI_STREAMS));
    tokio::spawn(async move {
        loop {
            match conn.accept_uni().await {
                Ok(mut uni) => {
                    let tx = uni_server_tx.clone();
                    let permit = match Arc::clone(&sem).acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => break, // semaphore closed
                    };
                    tokio::spawn(async move {
                        let _permit = permit; // held until task exits
                        loop {
                            match read_frame(&mut uni).await {
                                Ok(Some(frame)) => match decode_server(&frame) {
                                    Ok(msg) => {
                                        if tx.send(msg).is_err() {
                                            break;
                                        }
                                    }
                                    Err(e) => warn!("uni stream decode error: {e}"),
                                },
                                Ok(None) => break,
                                Err(e) => {
                                    warn!("uni stream read error: {e}");
                                    break;
                                }
                            }
                        }
                        debug!("Uni stream reader exited");
                    });
                }
                Err(e) => {
                    debug!("Uni stream accept ended: {e}");
                    break;
                }
            }
        }
    });

    ConnectResult::Connected(client_tx)
}

#[cfg(feature = "remote")]
fn build_quinn_client_config(host: &str, port: u16, accept_invalid: bool) -> quinn::ClientConfig {
    let addr_key = format!("{host}:{port}");

    // Load (or create empty) TOFU store from known_hosts.toml.
    let store = {
        let path = kmux_protocol::dirs::known_hosts_path()
            .inspect_err(|e| warn!("cannot determine known_hosts path: {e}"))
            .ok();
        let store = path.and_then(|p| {
            TofuStore::load(p)
                .inspect_err(|e| warn!("failed to load known_hosts: {e}"))
                .ok()
        });
        // Fall back to a temp-file-backed store if loading fails — still persists within
        // this process run but won't survive across restarts.
        match store {
            Some(s) => Arc::new(Mutex::new(s)),
            None => {
                // Can't get a usable path; use an ephemeral in-memory store.
                let tmp =
                    std::env::temp_dir().join(format!("kmux-tofu-{}.toml", std::process::id()));
                Arc::new(Mutex::new(TofuStore::load(tmp).unwrap_or_else(|_| {
                    TofuStore::load(std::env::temp_dir().join("kmux-tofu-fallback.toml"))
                        .unwrap_or_else(|_| unreachable!("empty TOFU store"))
                })))
            }
        }
    };

    let verifier = TofuVerifier::new(addr_key, "quic", store, accept_invalid);

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("valid QUIC client config");

    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(kmux_protocol::QUIC_IDLE_TIMEOUT_SECS))
            .unwrap(),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(
        kmux_protocol::QUIC_KEEP_ALIVE_SECS,
    )));
    config.transport_config(Arc::new(transport));

    config
}
