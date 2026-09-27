#[cfg(feature = "remote")]
use std::net::ToSocketAddrs;
#[cfg(feature = "remote")]
use std::sync::Arc;
#[cfg(feature = "remote")]
use std::time::Duration;

use kmux_protocol::messages::ClientMessage;
#[cfg(feature = "remote")]
use kmux_protocol::messages::{ClientCapabilities, ResumeFrom, ServerMessage};
#[cfg(feature = "remote")]
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::sync::mpsc;
#[cfg(feature = "remote")]
use tracing::{debug, warn};

/// How many server-initiated uni streams — one per attached pane — may be open
/// at once (issue #208). Each is read by its own task as soon as it arrives:
/// capping the readers below this once left every pane past the 64th unread,
/// so a busy one filled its window and was reset and re-attached every
/// 330 s. The cap is QUIC's own stream credit, which bounds a misbehaving
/// server; a client attaches only the panes it shows.
#[cfg(feature = "remote")]
pub const MAX_PANE_STREAMS: u32 = 4096;

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
    resume: Option<ResumeFrom>,
) -> ConnectResult {
    let Some(addr) = format!("{host}:{port}")
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    else {
        return ConnectResult::Failed(format!("cannot resolve {host}:{port}"));
    };

    let client_config = match build_quinn_client_config(&host, port, accept_invalid_certs) {
        Ok(config) => config,
        Err(e) => return ConnectResult::Failed(e),
    };

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
        crate::tcp_connect::send_auth_frame(&mut ctrl_send, token, capabilities, resume).await
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

    tokio::spawn(accept_pane_streams(conn, server_tx));

    ConnectResult::Connected(client_tx)
}

/// Accept the server's per-pane uni streams until the connection ends, each
/// read by its own task into `server_tx` — no pane waits for another's
/// stream to finish (see [`MAX_PANE_STREAMS`]).
#[cfg(feature = "remote")]
async fn accept_pane_streams(
    conn: quinn::Connection,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    loop {
        match conn.accept_uni().await {
            Ok(uni) => {
                tokio::spawn(read_pane_stream(uni, server_tx.clone()));
            }
            Err(e) => {
                debug!("Uni stream accept ended: {e}");
                break;
            }
        }
    }
}

/// Forward every frame of one pane stream to `tx` until it ends.
#[cfg(feature = "remote")]
async fn read_pane_stream(mut uni: quinn::RecvStream, tx: mpsc::UnboundedSender<ServerMessage>) {
    let ended = loop {
        let frame = match read_frame(&mut uni).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break "finished".to_string(),
            Err(e) => break format!("read error: {e}"),
        };
        match decode_server(&frame) {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    break "client gone".to_string();
                }
            }
            Err(e) => warn!("uni stream decode error: {e}"),
        }
    };
    debug!("uni stream reader exited: {ended}");
}

#[cfg(feature = "remote")]
fn build_quinn_client_config(
    host: &str,
    port: u16,
    accept_invalid: bool,
) -> Result<quinn::ClientConfig, String> {
    let addr_key = format!("{host}:{port}");
    let verifier = crate::tcp_connect::build_tofu_verifier(addr_key, "quic", accept_invalid)?;

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("valid QUIC client config");

    let mut config = quinn::ClientConfig::new(Arc::new(quic_crypto));
    config.transport_config(Arc::new(client_transport_config()));
    Ok(config)
}

/// The client's QUIC transport: kmux's idle timeout and keep-alive, and room
/// for [`MAX_PANE_STREAMS`] pane streams at once.
#[cfg(feature = "remote")]
fn client_transport_config() -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(kmux_sys::QUIC_IDLE_TIMEOUT_SECS))
            .unwrap(),
    ));
    transport.keep_alive_interval(Some(Duration::from_secs(kmux_sys::QUIC_KEEP_ALIVE_SECS)));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(MAX_PANE_STREAMS));
    transport
}

#[cfg(all(test, feature = "remote"))]
mod tests {
    use std::collections::HashSet;

    use kmux_protocol::encode_server;

    use super::*;

    /// A bound on every loopback wait: loopback takes milliseconds.
    const WAIT: Duration = Duration::from_secs(10);

    /// A QUIC client (with the production transport config) and server
    /// connected over loopback. A real connection because QUIC stream credit
    /// is what is under test; `duplex` has none.
    async fn quic_pair() -> anyhow::Result<(
        quinn::Endpoint,
        quinn::Endpoint,
        quinn::Connection,
        quinn::Connection,
    )> {
        use anyhow::Context as _;
        use kmux_sys::tls::{CertMaterial, TofuVerifier, build_server_config};

        let tls = build_server_config(CertMaterial::self_signed()?)?;
        let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
        let server = quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            "127.0.0.1:0".parse()?,
        )?;
        let addr = server.local_addr()?;

        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TofuVerifier::accept_invalid(
                addr.to_string(),
                "quic",
            )))
            .with_no_client_auth();
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(Arc::new(client_transport_config()));
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
        client.set_default_client_config(config);

        let connecting = client.connect(addr, "localhost")?;
        let accepting = async {
            match server.accept().await {
                Some(incoming) => incoming.await.map_err(anyhow::Error::from),
                None => Err(anyhow::anyhow!("the server endpoint closed")),
            }
        };
        let (client_conn, server_conn) =
            tokio::time::timeout(WAIT, async { tokio::join!(connecting, accepting) })
                .await
                .context("the handshake completes")?;
        Ok((client, server, client_conn?, server_conn?))
    }

    /// More panes than the old cap on readers (64) and than QUIC's default
    /// stream credit (100) stream at once, every stream held open as an
    /// attached pane's is: each is read (issue #208). Before, panes past the
    /// 64th went unread until another pane's stream ended.
    #[tokio::test]
    async fn every_open_pane_stream_is_read_at_once() -> anyhow::Result<()> {
        use anyhow::Context as _;

        const PANES: usize = 150;
        let (_client, _server, client_conn, server_conn) = quic_pair().await?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(accept_pane_streams(client_conn, tx));

        let mut open = Vec::new();
        for seq in 0..150_u64 {
            let mut uni = tokio::time::timeout(WAIT, server_conn.open_uni())
                .await
                .context("stream credit for every pane")??;
            write_frame(&mut uni, &encode_server(&ServerMessage::Pong { seq })?).await?;
            open.push(uni);
        }

        let mut seen = HashSet::new();
        tokio::time::timeout(WAIT, async {
            while seen.len() < PANES {
                if let Some(ServerMessage::Pong { seq }) = rx.recv().await {
                    seen.insert(seq);
                }
            }
        })
        .await
        .context("every pane's stream is read")?;
        assert_eq!(seen.len(), PANES);
        Ok(())
    }
}
