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
mod admission;

#[cfg(feature = "framing")]
pub use admission::{
    HandshakeLimits, HandshakeLimitsError, MAX_PENDING_HANDSHAKES,
    MAX_PENDING_HANDSHAKES_PER_SOURCE, QUIC_RETRY_ABOVE,
};
#[cfg(feature = "framing")]
pub use listener::{
    AcceptError, HANDSHAKE_TIMEOUT, IncomingSession, Listener, PeerInfo, PendingSession,
    SessionTransport, serve,
};

#[cfg(feature = "framing")]
mod listener {
    use std::future::Future;
    use std::net::{IpAddr, SocketAddr};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Duration;

    use thiserror::Error;

    use kmux_protocol::messages::TransportKind;

    use super::admission::{Admission, HandshakeLimits, InFlight};

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

    /// Pause after a failed accept before the next one.
    const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

    /// A connection a [`Listener`] accepted whose transport handshake has not
    /// run yet. The handshake is the part a peer can stall, so it runs in the
    /// connection's own task ([`PendingSession::establish`]), never on the
    /// accept loop, where one peer that stops mid-handshake used to block
    /// every later accept on the same listener.
    ///
    /// Dropping one refuses the connection: the socket closes, or a QUIC
    /// client is sent a refusal.
    pub struct PendingSession {
        /// Where the connection comes from, when it has an address (not UDS).
        source: Option<IpAddr>,
        stage: Stage,
    }

    /// What is left to do to establish a [`PendingSession`].
    enum Stage {
        /// A handshake to run.
        Handshake(Pin<Box<dyn Future<Output = Result<IncomingSession, AcceptError>> + Send>>),
        /// A QUIC connection attempt not yet accepted, so it can still be
        /// retried or refused without the endpoint keeping any state for it.
        #[cfg(feature = "quic")]
        Quic(Box<quinn::Incoming>),
    }

    impl PendingSession {
        /// A connection whose handshake is `handshake`.
        pub fn new(
            handshake: impl Future<Output = Result<IncomingSession, AcceptError>> + Send + 'static,
        ) -> Self {
            Self {
                source: None,
                stage: Stage::Handshake(Box::pin(handshake)),
            }
        }

        /// A connection with no handshake to run (UDS, plain TCP).
        pub fn ready(session: IncomingSession) -> Self {
            Self::new(std::future::ready(Ok(session)))
        }

        /// A QUIC connection attempt, to be accepted by
        /// [`PendingSession::establish`].
        #[cfg(feature = "quic")]
        pub fn quic(incoming: quinn::Incoming) -> Self {
            Self {
                source: Some(incoming.remote_address().ip()),
                stage: Stage::Quic(Box::new(incoming)),
            }
        }

        /// The same connection, counted against `peer`'s address by the
        /// per-source handshake bound.
        #[must_use]
        pub fn with_peer(mut self, peer: SocketAddr) -> Self {
            self.source = Some(peer.ip());
            self
        }

        /// The address the connection comes from, if it has one.
        pub fn source(&self) -> Option<IpAddr> {
            self.source
        }

        /// Whether this is a QUIC client that has not proved it can receive
        /// packets at its source address. quinn guarantees such a client may
        /// be sent a Retry.
        pub fn needs_address_validation(&self) -> bool {
            match &self.stage {
                Stage::Handshake(_) => false,
                #[cfg(feature = "quic")]
                Stage::Quic(incoming) => !incoming.remote_address_validated(),
            }
        }

        /// Ask a QUIC client to prove its address (a stateless Retry); it
        /// comes back as a new, validated connection attempt. Any other
        /// connection, or one that may not be retried, is refused.
        pub fn retry(self) {
            match self.stage {
                Stage::Handshake(_) => {}
                #[cfg(feature = "quic")]
                Stage::Quic(incoming) => {
                    if let Err(e) = (*incoming).retry() {
                        e.into_incoming().refuse();
                    }
                }
            }
        }

        /// Run the handshake, giving up after `timeout`.
        ///
        /// # Errors
        ///
        /// [`AcceptError::HandshakeTimeout`] when it takes longer than
        /// `timeout`, or the transport's own error when it fails.
        pub async fn establish(self, timeout: Duration) -> Result<IncomingSession, AcceptError> {
            #[cfg_attr(
                not(feature = "quic"),
                expect(
                    clippy::infallible_destructuring_match,
                    reason = "a second variant exists with the `quic` feature"
                )
            )]
            let handshake = match self.stage {
                Stage::Handshake(handshake) => handshake,
                #[cfg(feature = "quic")]
                Stage::Quic(incoming) => Box::pin(super::quic::establish(*incoming)),
            };
            tokio::time::timeout(timeout, handshake)
                .await
                .map_err(|_| AcceptError::HandshakeTimeout(timeout))?
        }
    }

    impl std::fmt::Debug for PendingSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PendingSession")
                .field("source", &self.source)
                .finish_non_exhaustive()
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
    /// times out drops that connection only.
    ///
    /// `limits` bounds the handshakes in flight, in all and per source
    /// (issue #207). A connection past either bound is refused at once: the
    /// accept loop never waits for a slot, so connections queue neither in
    /// the daemon nor in the kernel's backlog or the QUIC endpoint behind it.
    /// Under load a QUIC client with an unproven address is sent a stateless
    /// Retry before it is counted.
    pub async fn serve(
        mut listener: Box<dyn Listener>,
        handshake_timeout: Duration,
        limits: HandshakeLimits,
        on_session: Arc<dyn Fn(IncomingSession) + Send + Sync>,
    ) {
        let kind = listener.kind();
        let in_flight = InFlight::default();
        loop {
            match listener.accept().await {
                Ok(pending) => {
                    let source = pending.source();
                    match in_flight.admit(&limits, source, pending.needs_address_validation()) {
                        Admission::Admit(slot) => {
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
                        Admission::Retry => pending.retry(),
                        Admission::Refuse(why) => {
                            tracing::debug!(?source, ?why, "{kind} connection refused");
                            drop(pending);
                        }
                    }
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

        /// A listener that plays back `accepts`, then reports itself closed.
        /// Each accept yields first, as a real one waits for the next
        /// connection, so the handshakes spawned so far get to run.
        struct Scripted {
            accepts: VecDeque<Result<PendingSession, AcceptError>>,
        }

        impl Scripted {
            fn new(accepts: impl IntoIterator<Item = Result<PendingSession, AcceptError>>) -> Self {
                Self {
                    accepts: accepts.into_iter().collect(),
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
                let next = self.accepts.pop_front().unwrap_or(Err(AcceptError::Closed));
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    next
                })
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

        /// A connection whose handshake never finishes, and a receiver that
        /// resolves once the connection has been dropped: refused at once, or
        /// timed out.
        fn stalled() -> (PendingSession, tokio::sync::oneshot::Receiver<()>) {
            let (alive, dropped) = tokio::sync::oneshot::channel::<()>();
            let pending = PendingSession::new(async move {
                let _alive = alive;
                std::future::pending().await
            });
            (pending, dropped)
        }

        fn peer(ip: &str) -> SocketAddr {
            SocketAddr::new(ip.parse().unwrap(), 4433)
        }

        /// Whether the connection behind `dropped` has been dropped.
        fn is_dropped(dropped: &mut tokio::sync::oneshot::Receiver<()>) -> bool {
            dropped.try_recv() == Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        }

        async fn serve_scripted(
            accepts: impl IntoIterator<Item = Result<PendingSession, AcceptError>>,
            limits: HandshakeLimits,
        ) -> tokio::sync::mpsc::UnboundedReceiver<TransportKind> {
            let (tx, served) = tokio::sync::mpsc::unbounded_channel();
            serve(
                Box::new(Scripted::new(accepts)),
                HANDSHAKE_TIMEOUT,
                limits,
                Arc::new(move |session: IncomingSession| {
                    let _ = tx.send(session.kind());
                }),
            )
            .await;
            served
        }

        /// A failed accept costs one backoff and not the listener: the next
        /// connection is still handed on, and `serve` returns once the
        /// listener closes.
        #[tokio::test(start_paused = true)]
        async fn serve_backs_off_after_a_failed_accept_and_ends_when_the_listener_closes() {
            let started = tokio::time::Instant::now();
            let mut served = serve_scripted(
                [
                    Err(AcceptError::Io(std::io::Error::other("EMFILE"))),
                    Ok(PendingSession::ready(uds_session())),
                ],
                HandshakeLimits::DAEMON,
            )
            .await;

            assert!(started.elapsed() >= ACCEPT_ERROR_BACKOFF);
            assert_eq!(served.recv().await, Some(TransportKind::Uds));
        }

        /// A source with its share of handshakes stalled is refused at once,
        /// not queued, and another source is still admitted. The stalled ones
        /// hold their slots until they time out (issue #207).
        #[tokio::test(start_paused = true)]
        async fn a_source_past_its_share_is_refused_at_once_and_others_are_admitted() {
            let noisy = peer("192.0.2.1");
            let (a, mut a_dropped) = stalled();
            let (b, mut b_dropped) = stalled();
            let (c, mut c_dropped) = stalled();
            let (other, mut other_dropped) = stalled();
            let started = tokio::time::Instant::now();

            serve_scripted(
                [
                    Ok(a.with_peer(noisy)),
                    Ok(b.with_peer(noisy)),
                    Ok(c.with_peer(noisy)),
                    Ok(other.with_peer(peer("192.0.2.2"))),
                ],
                HandshakeLimits::new(8, 2, 8).unwrap(),
            )
            .await;

            assert_eq!(started.elapsed(), Duration::ZERO, "nothing waited");
            assert!(is_dropped(&mut c_dropped), "the third from one source");
            assert!(!is_dropped(&mut a_dropped));
            assert!(!is_dropped(&mut b_dropped));
            assert!(!is_dropped(&mut other_dropped), "another source");

            let timed_out = tokio::time::timeout(2 * HANDSHAKE_TIMEOUT, a_dropped).await;
            assert!(
                timed_out.expect("bounded").is_err(),
                "dropped, not answered"
            );
            assert!(
                started.elapsed() >= HANDSHAKE_TIMEOUT,
                "held until it timed out"
            );
        }

        /// Past the listener-wide bound everyone is refused at once, a local
        /// socket included.
        #[tokio::test(start_paused = true)]
        async fn a_full_listener_refuses_at_once() {
            let (a, mut a_dropped) = stalled();
            let (b, mut b_dropped) = stalled();
            serve_scripted(
                [Ok(a.with_peer(peer("192.0.2.1"))), Ok(b)],
                HandshakeLimits::new(1, 1, 1).unwrap(),
            )
            .await;
            assert!(!is_dropped(&mut a_dropped));
            assert!(is_dropped(&mut b_dropped), "refused, not queued");
        }

        /// A slot is given back when its handshake finishes, so a listener
        /// allowed one handshake at a time serves connection after connection.
        /// Only QUIC is ever asked to validate its address: a stream
        /// connection is never retried, even with a threshold of zero.
        #[tokio::test(start_paused = true)]
        async fn finished_handshakes_free_their_slots_and_streams_are_never_retried() {
            let tls_like = || Ok(PendingSession::ready(uds_session()).with_peer(peer("192.0.2.1")));
            let mut served = serve_scripted(
                [tls_like(), tls_like(), tls_like()],
                HandshakeLimits::new(1, 1, 0).unwrap(),
            )
            .await;
            for _ in 0..3 {
                let kind = tokio::time::timeout(HANDSHAKE_TIMEOUT, served.recv()).await;
                assert_eq!(kind.expect("served in turn"), Some(TransportKind::Uds));
            }
        }

        #[test]
        fn a_pending_session_carries_its_source_and_debugs_without_its_handshake() {
            let pending = PendingSession::new(std::future::pending());
            assert_eq!(pending.source(), None);
            assert!(!pending.needs_address_validation());
            assert_eq!(
                format!("{pending:?}"),
                "PendingSession { source: None, .. }"
            );
            let pending = pending.with_peer(peer("192.0.2.9"));
            assert_eq!(pending.source(), Some("192.0.2.9".parse().unwrap()));
        }
    }
}
