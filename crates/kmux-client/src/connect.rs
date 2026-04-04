use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

use kmux_protocol::messages::{ClientMessage, ServerMessage};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, warn};

/// Maximum number of concurrent server-initiated uni streams (one per attached session).
const MAX_UNI_STREAMS: usize = 64;

/// Outcome of a connection attempt.
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
pub async fn connect(
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
) -> ConnectResult {
    let addr = match format!("{host}:{port}")
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        Some(a) => a,
        None => return ConnectResult::Failed(format!("cannot resolve {host}:{port}")),
    };

    let client_config = build_quinn_client_config(accept_invalid_certs);

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
    if let Ok(bytes) = encode_client(&ClientMessage::Auth {
        token,
        protocol_version: kmux_protocol::messages::PROTOCOL_VERSION,
    }) && let Err(e) = write_frame(&mut ctrl_send, &bytes).await
    {
        return ConnectResult::Failed(format!("auth write failed: {e}"));
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

fn build_quinn_client_config(accept_invalid: bool) -> quinn::ClientConfig {
    let crypto = if accept_invalid {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            if let Err(e) = roots.add(cert) {
                warn!("failed to add native cert: {e}");
            }
        }
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    };

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("valid QUIC client config");

    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(300)).unwrap(),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(15)));
    config.transport_config(Arc::new(transport));

    config
}

/// A certificate verifier that accepts any certificate.
/// Use ONLY in development environments with self-signed certificates.
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dh_params: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dh_params: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}
