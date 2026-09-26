//! TCP and TCP+TLS transport listeners.
//!
//! Phase 3: `PlainTcpListener` — plain TCP, no TLS (used inside existing SSH tunnels).
//! Phase 4: `TlsTcpListener` — TCP with mandatory TLS (LAN / UDP-blocked internet).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use tokio::net::TcpListener;

use crate::transport::{
    AcceptError, IncomingSession, Listener, PeerInfo, PendingSession, SessionTransport,
};
use kmux_protocol::messages::TransportKind;

// ─── TlsTcpListener ──────────────────────────────────────────────────────────

/// Server-side TCP+TLS listener.
///
/// Accepts TCP connections and performs a TLS handshake before yielding an
/// `IncomingSession`. Used for LAN / UDP-blocked internet, and as the inner
/// transport for SSH `-L` tunnels (Phase 4+).
#[cfg(feature = "tcp-tls")]
pub struct TlsTcpListener {
    inner: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

#[cfg(feature = "tcp-tls")]
impl TlsTcpListener {
    /// Bind a TCP+TLS listener on `addr` using the provided `tls_config`.
    pub async fn bind(addr: SocketAddr, tls_config: rustls::ServerConfig) -> std::io::Result<Self> {
        use std::sync::Arc;
        Ok(Self {
            inner: TcpListener::bind(addr).await?,
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(tls_config)),
        })
    }

    /// Return the actual local port after binding.
    pub fn local_port(&self) -> std::io::Result<u16> {
        Ok(self.inner.local_addr()?.port())
    }

    /// Return the actual local address after binding.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

#[cfg(feature = "tcp-tls")]
impl Listener for TlsTcpListener {
    fn kind(&self) -> TransportKind {
        TransportKind::TcpTls
    }

    /// Accepts the TCP connection only; the TLS handshake is the returned
    /// [`PendingSession`]'s, so a client that never sends a `ClientHello`
    /// delays nobody but itself.
    fn accept(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<PendingSession, AcceptError>> + Send + '_>> {
        Box::pin(async move {
            let (stream, remote_addr) = self.inner.accept().await.map_err(AcceptError::Io)?;
            let acceptor = self.acceptor.clone();
            Ok(PendingSession::new(async move {
                let tls_stream = acceptor.accept(stream).await.map_err(AcceptError::Io)?;
                let conn_span = tracing::info_span!(
                    "connection",
                    transport = "tcp+tls",
                    remote = ?remote_addr,
                    conn_id = tracing::field::Empty,
                    client_id = tracing::field::Empty,
                );
                tracing::info!(parent: &conn_span, remote = ?remote_addr, "TCP+TLS connection accepted");
                let (read, write) = tokio::io::split(tls_stream);
                Ok(IncomingSession {
                    read: Box::new(read),
                    write: Box::new(write),
                    peer: PeerInfo {
                        addr: Some(remote_addr),
                    },
                    span: conn_span,
                    transport: SessionTransport::TcpTls,
                })
            }))
        })
    }
}

// ─── PlainTcpListener ─────────────────────────────────────────────────────────

/// Server-side plain-TCP listener (no TLS).
///
/// Used in the legacy plaintext path until Phase 4 mandates TLS.
/// Each accepted connection yields an `IncomingSession` with split I/O halves.
pub struct PlainTcpListener {
    inner: TcpListener,
}

impl PlainTcpListener {
    /// Bind a TCP listener on `addr`.
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        Ok(Self {
            inner: TcpListener::bind(addr).await?,
        })
    }

    /// Return the actual local port after binding.
    pub fn local_port(&self) -> std::io::Result<u16> {
        Ok(self.inner.local_addr()?.port())
    }

    /// Return the actual local address after binding.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Listener for PlainTcpListener {
    fn kind(&self) -> TransportKind {
        TransportKind::Tcp
    }

    fn accept(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<PendingSession, AcceptError>> + Send + '_>> {
        Box::pin(async move {
            let (stream, remote_addr) = self.inner.accept().await.map_err(AcceptError::Io)?;
            let conn_span = tracing::info_span!(
                "connection",
                transport = "tcp",
                remote = ?remote_addr,
                conn_id = tracing::field::Empty,
                client_id = tracing::field::Empty,
            );
            tracing::info!(parent: &conn_span, remote = ?remote_addr, "TCP connection accepted");
            let (read, write) = tokio::io::split(stream);
            Ok(PendingSession::ready(IncomingSession {
                read: Box::new(read),
                write: Box::new(write),
                peer: PeerInfo {
                    addr: Some(remote_addr),
                },
                span: conn_span,
                transport: SessionTransport::Tcp,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plain_tcp_listener_binds_random_port() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = PlainTcpListener::bind(addr).await.expect("should bind");
        let port = listener.local_port().unwrap();
        assert!(port > 0);
    }

    #[cfg(feature = "tcp-tls")]
    #[tokio::test]
    async fn tls_tcp_listener_binds_random_port() {
        use crate::tls::CertMaterial;
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let material = CertMaterial::self_signed().expect("self-signed cert");
        let tls_config = crate::tls::build_server_config(material).expect("server config");
        let listener = TlsTcpListener::bind(addr, tls_config)
            .await
            .expect("should bind");
        assert!(listener.local_port().unwrap() > 0);
    }

    /// A bound on real-socket waits; the handshake timeout under test is an
    /// hour, so reaching it would mean the accept loop was blocked.
    #[cfg(feature = "tcp-tls")]
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    #[cfg(feature = "tcp-tls")]
    async fn tls_listener() -> TlsTcpListener {
        use crate::tls::CertMaterial;
        let material = CertMaterial::self_signed().expect("self-signed cert");
        let tls_config = crate::tls::build_server_config(material).expect("server config");
        TlsTcpListener::bind("127.0.0.1:0".parse().unwrap(), tls_config)
            .await
            .expect("should bind")
    }

    /// One TCP client connects and never sends a `ClientHello`; a well-behaved
    /// TLS client that connects after it is still accepted. With the handshake
    /// on the accept loop, the first client blocked every later accept.
    #[cfg(feature = "tcp-tls")]
    #[tokio::test]
    async fn a_stalled_tls_handshake_does_not_delay_the_next_accept() {
        use std::sync::Arc;

        use rustls::pki_types::ServerName;

        use crate::tls::TofuVerifier;
        use crate::transport::serve;

        let listener = tls_listener().await;
        let addr = listener.local_addr().unwrap();
        let (tx, mut accepted) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(serve(
            Box::new(listener),
            std::time::Duration::from_secs(3600),
            Arc::new(move |session: IncomingSession| {
                let _ = tx.send(session.peer.addr);
            }),
        ));

        let _stalled = tokio::net::TcpStream::connect(addr).await.unwrap();

        let config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TofuVerifier::accept_invalid(
                addr.to_string(),
                "tcp+tls",
            )))
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let client_addr = tcp.local_addr().unwrap();
        let _tls = tokio::time::timeout(
            WAIT,
            tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(ServerName::try_from("localhost").unwrap(), tcp),
        )
        .await
        .expect("the handshake is not stuck behind the stalled client")
        .expect("TLS handshake");

        let peer = tokio::time::timeout(WAIT, accepted.recv())
            .await
            .expect("the second client is accepted")
            .expect("serve is running");
        assert_eq!(peer, Some(client_addr), "only the TLS client got through");
    }

    /// A handshake that never finishes is given up after the timeout, on the
    /// paused clock.
    #[cfg(feature = "tcp-tls")]
    #[tokio::test(start_paused = true)]
    async fn a_handshake_that_never_finishes_times_out() {
        let mut listener = tls_listener().await;
        let addr = listener.local_addr().unwrap();
        let _stalled = tokio::net::TcpStream::connect(addr).await.unwrap();
        let pending = listener.accept().await.expect("the TCP accept succeeds");

        let timeout = std::time::Duration::from_secs(10);
        let started = tokio::time::Instant::now();
        let result = pending.establish(timeout).await;
        assert!(
            matches!(result, Err(AcceptError::HandshakeTimeout(t)) if t == timeout),
            "expected a handshake timeout"
        );
        assert!(started.elapsed() >= timeout);
    }
}
