use std::net::ToSocketAddrs;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use smux_protocol::messages::{ClientMessage, ServerMessage};
use smux_protocol::{decode_server, encode_client};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};

/// Outcome of a connection attempt.
pub enum ConnectResult {
    /// Connected successfully; returns a sender for outbound messages.
    Connected(mpsc::UnboundedSender<ClientMessage>),
    /// Connection failed with an error description.
    Failed(String),
}

/// Establish a TLS+WebSocket connection to `host:port` and authenticate with
/// `token`. The `server_tx` channel is used to send `ServerMessage` values back
/// into the iced application loop.
///
/// This function runs two background tasks:
/// - A **reader task**: decodes incoming `ServerMessage` frames and sends them
///   via `server_tx`.
/// - A **writer task**: receives `ClientMessage` values from the returned sender
///   and encodes them as WebSocket binary frames.
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

    let tcp = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return ConnectResult::Failed(format!("TCP connect failed: {e}")),
    };

    let tls_config = build_tls_config(accept_invalid_certs);
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = match ServerName::try_from(host.clone()) {
        Ok(n) => n,
        Err(_) => return ConnectResult::Failed(format!("invalid server name: {host}")),
    };

    let tls_stream = match connector.connect(server_name, tcp).await {
        Ok(s) => s,
        Err(e) => return ConnectResult::Failed(format!("TLS connect failed: {e}")),
    };

    let url = format!("wss://{host}:{port}/");
    let (ws_stream, _response) = match tokio_tungstenite::client_async(url, tls_stream).await {
        Ok(pair) => pair,
        Err(e) => return ConnectResult::Failed(format!("WebSocket upgrade failed: {e}")),
    };

    let (mut ws_sink, mut ws_stream) = ws_stream.split();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

    // Authenticate immediately
    if let Ok(bytes) = encode_client(&ClientMessage::Auth { token }) {
        let _ = ws_sink.send(Message::Binary(bytes.into())).await;
    }

    // Writer task: drain client_rx and send frames
    let writer_handle = tokio::spawn(async move {
        while let Some(msg) = client_rx.recv().await {
            match encode_client(&msg) {
                Ok(bytes) => {
                    if ws_sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("encode error: {e}"),
            }
        }
        debug!("Writer task exited");
    });

    // Reader task: decode incoming frames and forward as ServerMessage
    tokio::spawn(async move {
        while let Some(frame) = ws_stream.next().await {
            match frame {
                Ok(Message::Binary(data)) => match decode_server(&data) {
                    Ok(msg) => {
                        if server_tx.send(msg).is_err() {
                            break;
                        }
                    }
                    Err(e) => warn!("decode error: {e}"),
                },
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    warn!("WebSocket error: {e}");
                    break;
                }
            }
        }
        writer_handle.abort();
        debug!("Reader task exited");
    });

    ConnectResult::Connected(client_tx)
}

fn build_tls_config(accept_invalid: bool) -> rustls::ClientConfig {
    if accept_invalid {
        // Development mode: accept any certificate (self-signed)
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
    }
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
