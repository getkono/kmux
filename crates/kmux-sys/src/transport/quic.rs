/// QUIC idle timeout in seconds (shared by client and server transport configs).
pub const QUIC_IDLE_TIMEOUT_SECS: u64 = 300;
/// QUIC keep-alive interval in seconds (shared by client and server transport configs).
pub const QUIC_KEEP_ALIVE_SECS: u64 = 15;

/// `QuicListener`: accepts QUIC connections from a `quinn::Endpoint` and
/// yields `IncomingSession` values for dispatch into `run_client_session`.
///
/// Feature-gated on `quic`.
#[cfg(feature = "quic")]
mod quic_listener {
    use std::future::Future;
    use std::pin::Pin;

    use quinn::Endpoint;

    use crate::transport::{
        AcceptError, IncomingSession, Listener, PeerInfo, PendingSession, SessionTransport,
    };
    use kmux_protocol::messages::TransportKind;

    /// Server-side QUIC transport listener.
    ///
    /// Wraps a `quinn::Endpoint` and accepts bidirectional control streams.
    /// The `quinn::Connection` is stored in `IncomingSession.extra` for use by
    /// `QuicAttacher` at the dispatch site.
    pub struct QuicListener {
        endpoint: Endpoint,
    }

    impl QuicListener {
        pub fn new(endpoint: Endpoint) -> Self {
            Self { endpoint }
        }
    }

    impl Listener for QuicListener {
        fn kind(&self) -> TransportKind {
            TransportKind::Quic
        }

        fn accept(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Result<PendingSession, AcceptError>> + Send + '_>>
        {
            let endpoint = self.endpoint.clone();
            Box::pin(async move {
                let incoming = endpoint.accept().await.ok_or(AcceptError::Closed)?;
                // Not accepted yet: the accept loop decides whether to retry,
                // refuse or admit it (issue #207). The QUIC handshake and the
                // control stream belong to the connection's own task, like
                // TLS's (issue #206).
                Ok(PendingSession::quic(incoming))
            })
        }
    }

    /// Complete an incoming QUIC connection: its handshake, then its
    /// bidirectional control stream.
    pub(in crate::transport) async fn establish(
        incoming: quinn::Incoming,
    ) -> Result<IncomingSession, AcceptError> {
        let conn = incoming
            .await
            .map_err(|e| AcceptError::Transport(e.to_string()))?;
        let remote = conn.remote_address();

        let conn_span = tracing::info_span!(
            "connection",
            transport = "quic",
            remote = %remote,
            conn_id = tracing::field::Empty,
            client_id = tracing::field::Empty,
        );
        tracing::info!(parent: &conn_span, "QUIC connection from {remote}");

        let (ctrl_send, ctrl_recv) = conn
            .accept_bi()
            .await
            .map_err(|e| AcceptError::Transport(format!("accept bi: {e}")))?;

        Ok(IncomingSession {
            read: Box::new(ctrl_recv),
            write: Box::new(ctrl_send),
            peer: PeerInfo { addr: Some(remote) },
            span: conn_span,
            transport: SessionTransport::Quic(conn),
        })
    }
}

#[cfg(feature = "quic")]
pub use quic_listener::QuicListener;
#[cfg(feature = "quic")]
pub(super) use quic_listener::establish;

#[cfg(test)]
#[cfg(feature = "quic")]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use kmux_protocol::messages::TransportKind;
    use tokio::io::AsyncReadExt;

    use super::QuicListener;
    use crate::tls::{CertMaterial, TofuVerifier, build_server_config};
    use crate::transport::{AcceptError, HANDSHAKE_TIMEOUT, Listener};

    /// A bound on real-socket waits over loopback.
    const WAIT: Duration = Duration::from_secs(10);

    fn server_endpoint() -> quinn::Endpoint {
        let tls = build_server_config(CertMaterial::self_signed().expect("self-signed cert"))
            .expect("server config");
        let crypto =
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("QUIC server crypto");
        quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            "127.0.0.1:0".parse().unwrap(),
        )
        .expect("bind the server endpoint")
    }

    fn client_endpoint(server: SocketAddr) -> quinn::Endpoint {
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TofuVerifier::accept_invalid(
                server.to_string(),
                "quic",
            )))
            .with_no_client_auth();
        let crypto =
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("QUIC client crypto");
        let mut endpoint =
            quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("bind the client");
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        endpoint
    }

    /// `accept` hands back the connection before its handshake; `establish`
    /// completes it and yields the client's first bidirectional stream as the
    /// control stream, with the client's address and the QUIC connection.
    #[tokio::test]
    async fn an_accepted_connection_establishes_with_its_control_stream() {
        let server = server_endpoint();
        let addr = server.local_addr().unwrap();
        let mut listener = QuicListener::new(server);
        assert_eq!(listener.kind(), TransportKind::Quic);

        let client = client_endpoint(addr);
        let client_addr = client.local_addr().unwrap();
        let client_side = tokio::spawn(async move {
            let conn = client
                .connect(addr, "localhost")
                .expect("connect")
                .await
                .expect("client handshake");
            let (mut send, recv) = conn.open_bi().await.expect("open the control stream");
            // A stream is announced to the peer by its first bytes.
            send.write_all(b"hello").await.expect("write");
            (client, conn, send, recv)
        });

        let pending = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("a connection arrives")
            .expect("accept");
        let mut session = tokio::time::timeout(WAIT, pending.establish(HANDSHAKE_TIMEOUT))
            .await
            .expect("the handshake completes")
            .expect("establish");
        assert_eq!(session.kind(), TransportKind::Quic);
        assert_eq!(session.peer.addr, Some(client_addr));

        let mut hello = [0u8; 5];
        tokio::time::timeout(WAIT, session.read.read_exact(&mut hello))
            .await
            .expect("the control stream carries the client's bytes")
            .expect("read");
        assert_eq!(&hello, b"hello");
        drop(client_side.await.expect("client"));
    }

    /// A closed endpoint ends the accept loop.
    #[tokio::test]
    async fn accept_on_a_closed_endpoint_reports_closed() {
        let server = server_endpoint();
        let mut listener = QuicListener::new(server.clone());
        server.close(0u32.into(), b"done");
        assert!(matches!(listener.accept().await, Err(AcceptError::Closed)));
    }

    /// Start a client connecting to `server` that opens its control stream,
    /// and return what the attempt came to.
    fn connect_and_open(
        server: SocketAddr,
    ) -> tokio::task::JoinHandle<Result<(quinn::Endpoint, quinn::Connection), String>> {
        let client = client_endpoint(server);
        tokio::spawn(async move {
            let conn = client
                .connect(server, "localhost")
                .map_err(|e| e.to_string())?
                .await
                .map_err(|e| e.to_string())?;
            let (mut send, _recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
            send.write_all(b"hello").await.map_err(|e| e.to_string())?;
            Ok((client, conn))
        })
    }

    /// A first connection attempt has not proved its address; a Retry sends
    /// the client back with a token, and its second attempt has, and
    /// establishes (issue #207).
    #[tokio::test]
    async fn a_retried_client_comes_back_validated_and_establishes() {
        let server = server_endpoint();
        let addr = server.local_addr().unwrap();
        let mut listener = QuicListener::new(server);
        let client_side = connect_and_open(addr);

        let first = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("an attempt arrives")
            .expect("accept");
        assert!(first.needs_address_validation(), "not yet validated");
        assert_eq!(first.source(), Some("127.0.0.1".parse().unwrap()));
        first.retry();

        let second = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("the client retries")
            .expect("accept");
        assert!(!second.needs_address_validation(), "validated by the retry");
        tokio::time::timeout(WAIT, second.establish(HANDSHAKE_TIMEOUT))
            .await
            .expect("the handshake completes")
            .expect("establish");
        let connected = tokio::time::timeout(WAIT, client_side).await;
        connected
            .expect("bounded")
            .expect("client")
            .expect("connected");
    }

    /// Dropping an attempt refuses it: the client is told at once rather than
    /// left to time out (issue #207).
    #[tokio::test]
    async fn a_dropped_attempt_is_refused_at_once() {
        let server = server_endpoint();
        let addr = server.local_addr().unwrap();
        let mut listener = QuicListener::new(server);
        let client_side = connect_and_open(addr);

        let attempt = tokio::time::timeout(WAIT, listener.accept())
            .await
            .expect("an attempt arrives")
            .expect("accept");
        drop(attempt);

        let refused = tokio::time::timeout(WAIT, client_side)
            .await
            .expect("refused well inside the idle timeout")
            .expect("client");
        let err = refused.expect_err("the connect fails");
        assert!(err.contains("refused"), "refused, got: {err}");
    }

    /// `serve` retries unvalidated QUIC clients when told to (a threshold of
    /// zero), and a retried client still gets through to `on_session`.
    #[tokio::test]
    async fn serve_admits_a_client_after_retrying_it() {
        use crate::transport::{HandshakeLimits, IncomingSession, serve};

        let server = server_endpoint();
        let addr = server.local_addr().unwrap();
        let (tx, mut served) = tokio::sync::mpsc::unbounded_channel();
        let serving = tokio::spawn(serve(
            Box::new(QuicListener::new(server)),
            HANDSHAKE_TIMEOUT,
            HandshakeLimits::new(4, 4, 0).unwrap(),
            Arc::new(move |session: IncomingSession| {
                let _ = tx.send(session.kind());
            }),
        ));
        let client_side = connect_and_open(addr);

        let kind = tokio::time::timeout(WAIT, served.recv()).await;
        assert_eq!(kind.expect("served"), Some(TransportKind::Quic));
        let connected = tokio::time::timeout(WAIT, client_side).await;
        connected
            .expect("bounded")
            .expect("client")
            .expect("connected");
        serving.abort();
    }
}
