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
pub use listener::{
    AcceptError, HANDSHAKE_TIMEOUT, IncomingSession, Listener, MAX_PENDING_HANDSHAKES, PeerInfo,
    PendingSession, SessionTransport, serve,
};

#[cfg(feature = "framing")]
mod listener {
    use std::future::Future;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

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
        /// The transport handshake (TLS, QUIC) did not finish in time.
        #[error("handshake did not finish within {0:?}")]
        HandshakeTimeout(Duration),
    }

    // ─── IncomingSession ──────────────────────────────────────────────────────

    /// A newly accepted connection, transport-agnostic.
    ///
    /// `read` and `write` carry the control-stream I/O halves.  For QUIC these
    /// are the first accepted bidirectional stream; for TCP/UDS they are the
    /// socket halves after `split()`.
    ///
    /// `transport` names the transport and carries whatever the dispatcher
    /// needs beyond the I/O halves.
    pub struct IncomingSession {
        pub read: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
        pub write: Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
        pub peer: PeerInfo,
        pub span: tracing::Span,
        /// The transport, with its transport-specific state.
        pub transport: SessionTransport,
    }

    impl IncomingSession {
        /// Which transport this session arrived on.
        pub fn kind(&self) -> TransportKind {
            self.transport.kind()
        }
    }

    /// The transport an [`IncomingSession`] arrived on, carrying the state that
    /// transport needs.
    ///
    /// This was a `kind: TransportKind` field beside a `Box<dyn Any + Send>`
    /// that the dispatcher downcast — a pairing nothing enforced, and whose one
    /// consumer wrote `.downcast::<quinn::Connection>().expect(..)`, so any
    /// listener that produced `kind: Quic` without a connection would take the
    /// daemon down. Splitting that into a `kind` field and a state enum still
    /// let `kind: Quic` sit beside "no state" and quietly take the stream path.
    /// With one enum there is no second field to disagree: a QUIC session
    /// without its connection cannot be constructed.
    #[derive(Debug)]
    pub enum SessionTransport {
        /// Unix domain socket.
        Uds,
        /// Plain TCP.
        Tcp,
        /// TCP with TLS.
        TcpTls,
        /// QUIC, with the connection the pane attacher needs in order to open
        /// per-pane unidirectional streams.
        #[cfg(feature = "quic")]
        Quic(quinn::Connection),
    }

    impl SessionTransport {
        /// The transport's wire-level kind.
        pub fn kind(&self) -> TransportKind {
            match self {
                Self::Uds => TransportKind::Uds,
                Self::Tcp => TransportKind::Tcp,
                Self::TcpTls => TransportKind::TcpTls,
                #[cfg(feature = "quic")]
                Self::Quic(_) => TransportKind::Quic,
            }
        }
    }

    // ─── PendingSession ───────────────────────────────────────────────────────

    /// Longest a transport handshake (TLS, QUIC) may take before the
    /// connection is dropped (issue #206). Generous for a slow link; a peer
    /// that has not finished by then is not going to.
    pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Most connections a listener handshakes at once (issue #207). Each
    /// in-flight handshake is a task, a socket and TLS state; past this many,
    /// the listener stops accepting until one finishes or times out, and
    /// further connections wait in the kernel's accept backlog (TCP, UDS) or
    /// the QUIC endpoint's queue instead of in the daemon's memory. Far above
    /// what legitimate clients produce, which reconnect a handful at a time.
    pub const MAX_PENDING_HANDSHAKES: usize = 64;

    /// Pause after a failed accept before the next one.
    const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

    /// A connection a [`Listener`] accepted whose transport handshake has not
    /// run yet. The handshake is the part a peer can stall, so it runs in the
    /// connection's own task ([`PendingSession::establish`]), never on the
    /// accept loop, where one peer that stops mid-handshake used to block
    /// every later accept on the same listener.
    pub struct PendingSession {
        handshake: Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send>>,
    }

    impl PendingSession {
        /// A connection whose handshake is `handshake`.
        pub fn new(
            handshake: impl Future<Output = Result<IncomingSession, AcceptError>> + Send + 'static,
        ) -> Self {
            Self {
                handshake: Box::pin(handshake),
            }
        }

        /// A connection with no handshake to run (UDS, plain TCP).
        pub fn ready(session: IncomingSession) -> Self {
            Self::new(std::future::ready(Ok(session)))
        }

        /// Run the handshake, giving up after `timeout`.
        ///
        /// # Errors
        ///
        /// [`AcceptError::HandshakeTimeout`] when it takes longer than
        /// `timeout`, or the transport's own error when it fails.
        pub async fn establish(self, timeout: Duration) -> Result<IncomingSession, AcceptError> {
            tokio::time::timeout(timeout, self.handshake)
                .await
                .map_err(|_| AcceptError::HandshakeTimeout(timeout))?
        }
    }

    impl std::fmt::Debug for PendingSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PendingSession").finish_non_exhaustive()
        }
    }

    // ─── Listener ─────────────────────────────────────────────────────────────

    /// A server-side transport listener.
    ///
    /// `accept()` returns a `Pin<Box<dyn Future>>` to remain object-safe so
    /// implementations can be stored in `Vec<Box<dyn Listener>>`.
    pub trait Listener: Send {
        fn kind(&self) -> TransportKind;

        /// Accept the next connection. Resolves as soon as the transport has
        /// one, before any handshake: that is [`PendingSession::establish`].
        fn accept(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Result<PendingSession, AcceptError>> + Send + '_>>;
    }

    /// Accept connections on `listener` until it closes, completing each
    /// one's handshake in its own task under `handshake_timeout` and handing
    /// the established session to `on_session`. A handshake that fails or
    /// times out drops that connection only. At most `max_handshakes` run at
    /// once: with that many in flight, the next accept waits for one of them
    /// to end.
    pub async fn serve(
        mut listener: Box<dyn Listener>,
        handshake_timeout: Duration,
        max_handshakes: usize,
        on_session: Arc<dyn Fn(IncomingSession) + Send + Sync>,
    ) {
        let kind = listener.kind();
        let slots = Arc::new(tokio::sync::Semaphore::new(max_handshakes));
        loop {
            // The semaphore is never closed, so this only waits.
            let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                break;
            };
            match listener.accept().await {
                Ok(pending) => {
                    let on_session = Arc::clone(&on_session);
                    tokio::spawn(async move {
                        // Held until the handshake ends, either way.
                        let _slot = slot;
                        match pending.establish(handshake_timeout).await {
                            Ok(session) => on_session(session),
                            Err(e) => tracing::warn!("{kind} handshake failed: {e}"),
                        }
                    });
                }
                Err(AcceptError::Closed) => break,
                Err(e) => {
                    tracing::warn!("accept error on {kind} listener: {e}");
                    // Out of file descriptors fails every accept at once:
                    // pause rather than spin until one is released.
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;

        use super::*;

        /// A listener that plays back `accepts`, then reports itself closed,
        /// counting the accepts it was asked for.
        struct Scripted {
            accepts: VecDeque<Result<PendingSession, AcceptError>>,
            asked: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Scripted {
            fn new(accepts: impl IntoIterator<Item = Result<PendingSession, AcceptError>>) -> Self {
                Self {
                    accepts: accepts.into_iter().collect(),
                    asked: Arc::default(),
                }
            }
        }

        impl Listener for Scripted {
            fn kind(&self) -> TransportKind {
                TransportKind::Uds
            }

            fn accept(
                &mut self,
            ) -> Pin<Box<dyn Future<Output = Result<PendingSession, AcceptError>> + Send + '_>>
            {
                self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let next = self.accepts.pop_front().unwrap_or(Err(AcceptError::Closed));
                Box::pin(std::future::ready(next))
            }
        }

        fn uds_session() -> IncomingSession {
            let (read, write) = tokio::io::split(tokio::io::duplex(64).0);
            IncomingSession {
                read: Box::new(read),
                write: Box::new(write),
                peer: PeerInfo { addr: None },
                span: tracing::Span::none(),
                transport: SessionTransport::Uds,
            }
        }

        /// A failed accept costs one backoff and not the listener: the next
        /// connection is still handed on, and `serve` returns once the
        /// listener closes.
        #[tokio::test(start_paused = true)]
        async fn serve_backs_off_after_a_failed_accept_and_ends_when_the_listener_closes() {
            let listener = Scripted::new([
                Err(AcceptError::Io(std::io::Error::other("EMFILE"))),
                Ok(PendingSession::ready(uds_session())),
            ]);
            let (tx, mut served) = tokio::sync::mpsc::unbounded_channel();
            let started = tokio::time::Instant::now();

            serve(
                Box::new(listener),
                HANDSHAKE_TIMEOUT,
                MAX_PENDING_HANDSHAKES,
                Arc::new(move |session: IncomingSession| {
                    let _ = tx.send(session.kind());
                }),
            )
            .await;

            assert!(started.elapsed() >= ACCEPT_ERROR_BACKOFF);
            assert_eq!(served.recv().await, Some(TransportKind::Uds));
        }

        /// With `max_handshakes` handshakes stalled, the listener accepts
        /// nothing more until one of them times out and frees its slot
        /// (issue #207).
        #[tokio::test(start_paused = true)]
        async fn serve_accepts_no_more_than_the_cap_while_handshakes_are_pending() {
            let stalled = || Ok(PendingSession::new(std::future::pending()));
            let listener = Scripted::new([stalled(), stalled(), stalled()]);
            let asked = Arc::clone(&listener.asked);
            let accepts = move || asked.load(std::sync::atomic::Ordering::SeqCst);
            let started = tokio::time::Instant::now();
            let serving = tokio::spawn(serve(
                Box::new(listener),
                HANDSHAKE_TIMEOUT,
                2,
                Arc::new(|_: IncomingSession| {}),
            ));

            tokio::time::sleep(HANDSHAKE_TIMEOUT / 2).await;
            assert_eq!(accepts(), 2, "the third accept waits for a slot");

            serving.await.unwrap();
            assert!(started.elapsed() >= HANDSHAKE_TIMEOUT);
            assert_eq!(accepts(), 4, "the rest once the stalled ones timed out");
        }

        #[test]
        fn a_pending_session_debugs_without_its_handshake() {
            let pending = PendingSession::new(std::future::pending());
            assert_eq!(format!("{pending:?}"), "PendingSession { .. }");
        }
    }
}
