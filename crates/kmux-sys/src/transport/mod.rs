//! Transport-layer abstractions: the server-side `Listener` trait, the
//! per-transport connect/accept implementations, and the endpoints a server
//! advertises for the data plane.

use kmux_protocol::messages::TransportKind;

/// A transport endpoint advertised by the server after authentication.
///
/// Populated from `AuthResult`. The client uses this list to open and rank
/// data-plane transports once bootstrap has produced an authenticated channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAdvert {
    /// Transport kind for this endpoint.
    pub kind: TransportKind,
    /// Connection address: `"host:port"` for QUIC/TLS-TCP, absolute path for UDS.
    pub address: String,
}

#[cfg(test)]
mod endpoint_advert_tests {
    use super::EndpointAdvert;
    use kmux_protocol::messages::TransportKind;

    #[test]
    fn two_adverts_are_equal_when_kind_and_address_match() {
        let advert = |address: &str| EndpointAdvert {
            kind: TransportKind::Quic,
            address: address.to_owned(),
        };
        assert_eq!(advert("host:8443"), advert("host:8443"));
        assert_ne!(advert("host:8443"), advert("host:8444"));
    }
}

pub mod quic;

#[cfg(feature = "framing")]
pub mod tcp_tls;

#[cfg(feature = "uds")]
pub mod uds;

#[cfg(feature = "framing")]
pub use listener::{AcceptError, IncomingSession, Listener, PeerInfo};

#[cfg(feature = "framing")]
mod listener {
    use std::any::Any;
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;

    use thiserror::Error;

    use kmux_protocol::messages::TransportKind;

    // ─── PeerInfo ─────────────────────────────────────────────────────────────

    /// Peer connection metadata.
    #[derive(Debug, Clone)]
    pub struct PeerInfo {
        pub addr: Option<SocketAddr>,
    }

    // ─── AcceptError ──────────────────────────────────────────────────────────

    /// Error variants for accepting a new session.
    #[derive(Debug, Error)]
    pub enum AcceptError {
        #[error("listener closed")]
        Closed,
        #[error("I/O error: {0}")]
        Io(#[from] std::io::Error),
        #[error("transport error: {0}")]
        Transport(String),
    }

    // ─── IncomingSession ──────────────────────────────────────────────────────

    /// A newly accepted connection, transport-agnostic.
    ///
    /// `read` and `write` carry the control-stream I/O halves.  For QUIC these
    /// are the first accepted bidirectional stream; for TCP/UDS they are the
    /// socket halves after `split()`.
    ///
    /// `extra` holds transport-specific state the dispatcher needs at the call
    /// site (e.g. `quinn::Connection` for `QuicAttacher`).  Callers downcast it.
    pub struct IncomingSession {
        pub read: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        pub write: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        pub kind: TransportKind,
        pub peer: PeerInfo,
        pub span: tracing::Span,
        /// Transport-specific state; downcast at the dispatch site.
        pub extra: Box<dyn Any + Send>,
    }

    // ─── Listener ─────────────────────────────────────────────────────────────

    /// A server-side transport listener.
    ///
    /// `accept()` returns a `Pin<Box<dyn Future>>` to remain object-safe so
    /// implementations can be stored in `Vec<Box<dyn Listener>>`.
    pub trait Listener: Send {
        fn kind(&self) -> TransportKind;

        /// Accept the next incoming session.
        fn accept(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send + '_>>;
    }
}
