//! TCP and TCP+TLS transport listeners.
//!
//! Phase 3: `PlainTcpListener` — plain TCP, no TLS (used inside existing SSH tunnels).
//! Phase 4: `TlsTcpListener` — TCP with mandatory TLS (LAN / UDP-blocked internet).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;

use tokio::net::TcpListener;

use crate::transport::{AcceptError, IncomingSession, Listener, PeerInfo, SessionExtra};
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

    fn accept(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send + '_>> {
        Box::pin(async move {
            let (stream, remote_addr) = self.inner.accept().await.map_err(AcceptError::Io)?;
            let tls_stream = self
                .acceptor
                .accept(stream)
                .await
                .map_err(AcceptError::Io)?;
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
                kind: TransportKind::TcpTls,
                peer: PeerInfo {
                    addr: Some(remote_addr),
                },
                span: conn_span,
                extra: SessionExtra::None,
            })
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
    ) -> Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send + '_>> {
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
            Ok(IncomingSession {
                read: Box::new(read),
                write: Box::new(write),
                kind: TransportKind::Tcp,
                peer: PeerInfo {
                    addr: Some(remote_addr),
                },
                span: conn_span,
                extra: SessionExtra::None, // no transport-specific extra state for plain TCP
            })
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
}
