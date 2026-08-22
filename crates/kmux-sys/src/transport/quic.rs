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

    use crate::transport::{AcceptError, IncomingSession, Listener, PeerInfo, SessionExtra};
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
        ) -> Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send + '_>>
        {
            let endpoint = self.endpoint.clone();
            Box::pin(async move {
                let incoming = endpoint.accept().await.ok_or(AcceptError::Closed)?;
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
                    kind: TransportKind::Quic,
                    peer: PeerInfo { addr: Some(remote) },
                    span: conn_span,
                    extra: SessionExtra::Quic(conn),
                })
            })
        }
    }
}

#[cfg(feature = "quic")]
pub use quic_listener::QuicListener;
