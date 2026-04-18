//! Bootstrap strategies and the `bootstrap_race` runner.
//!
//! Phase 5: implements the four bootstrap strategies defined in the plan and
//! provides `bootstrap_race`, which runs them concurrently (via
//! `futures::stream::FuturesUnordered`) and returns the first successful
//! `SessionContext`.
//!
//! Strategies (in typical priority order):
//!
//! 1. [`UdsLocalBootstrap`] — local Unix-domain-socket data connection to a
//!    running daemon. Wins in microseconds when the daemon is running locally.
//! 2. [`QuicDirectBootstrap`] — direct QUIC handshake to the server.
//! 3. [`TlsTcpDirectBootstrap`] — direct TLS-TCP handshake to the server.
//! 4. [`SshBootstrap`] — `kmuxd probe-or-start` over SSH, then TCP+TLS over
//!    an SSH `-L` tunnel (mandatory TLS, no plain-TCP path).

use std::pin::Pin;

use futures::stream::{FuturesUnordered, StreamExt};
use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, ConnectionId, PROTOCOL_VERSION, ServerMessage, TransportKind,
};
use kmux_protocol::transport::bootstrap::{
    Bootstrap, BootstrapError, EndpointAdvert, SessionContext,
};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Perform the auth handshake on a connected stream, reading `AuthResult` synchronously.
///
/// Steps:
/// 1. Send `ClientMessage::Auth` frame.
/// 2. Read one frame from the stream.
/// 3. Decode it as `ServerMessage::AuthResult`.
/// 4. On success: forward the full `AuthResult` intact via `server_tx` so that
///    `SessionManager` sees `client_id`, `server_version`, and `connection_id`.
/// 5. Return `(connection_id, server_endpoints)`.
pub(crate) async fn perform_auth_handshake<W, R>(
    write_half: &mut W,
    read_half: &mut R,
    token: String,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
    server_tx: &mpsc::UnboundedSender<ServerMessage>,
) -> Result<(ConnectionId, Vec<EndpointAdvert>), BootstrapError>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    let auth_bytes = encode_client(&ClientMessage::Auth {
        token,
        protocol_version: PROTOCOL_VERSION,
        capabilities,
        connection_id,
    })
    .map_err(|e| BootstrapError::ConnectionFailed(format!("auth encode: {e}")))?;

    write_frame(write_half, &auth_bytes)
        .await
        .map_err(|e| BootstrapError::ConnectionFailed(format!("auth write: {e}")))?;

    let frame = read_frame(read_half)
        .await
        .map_err(|e| BootstrapError::ConnectionFailed(format!("auth read: {e}")))?
        .ok_or_else(|| BootstrapError::ConnectionFailed("connection closed during auth".into()))?;

    let msg = decode_server(&frame)
        .map_err(|e| BootstrapError::ConnectionFailed(format!("auth decode: {e}")))?;

    match msg {
        ServerMessage::AuthResult {
            success: true,
            connection_id: Some(conn_id),
            ..
        } => {
            let _ = server_tx.send(msg);
            Ok((conn_id, Vec::new()))
        }
        ServerMessage::AuthResult {
            success: true,
            connection_id: None,
            ..
        } => Err(BootstrapError::AuthFailed {
            reason: "server sent success=true but no connection_id".into(),
        }),
        ServerMessage::AuthResult {
            success: false,
            reason,
            ..
        } => {
            let reason = reason.unwrap_or_else(|| "rejected".into());
            if reason.starts_with("protocol version mismatch:") {
                Err(BootstrapError::VersionMismatch {
                    client: PROTOCOL_VERSION,
                    server: 0,
                })
            } else {
                Err(BootstrapError::AuthFailed { reason })
            }
        }
        _ => Err(BootstrapError::ConnectionFailed(
            "expected AuthResult, got a different message".into(),
        )),
    }
}

// ─── UdsLocalBootstrap ────────────────────────────────────────────────────────

/// Bootstrap via the local daemon's Unix data socket.
///
/// Calls [`kmux_client::daemon::ensure_compatible_daemon`] to start the daemon
/// if not running and verify its protocol version matches ours, then connects
/// to `daemon-data.sock` and performs the auth handshake.
/// Wins in microseconds when the daemon is already running.
pub struct UdsLocalBootstrap {
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
}

impl Bootstrap for UdsLocalBootstrap {
    fn name(&self) -> &'static str {
        "uds-local"
    }

    fn try_bootstrap(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<SessionContext, BootstrapError>> + Send + '_>,
    > {
        Box::pin(async move {
            // Ensure the daemon is running and get its status.
            let status = crate::daemon::ensure_daemon()
                .await
                .map_err(|e| BootstrapError::ConnectionFailed(format!("daemon start: {e}")))?;

            // Version gate: refuse before attempting a data-plane connection.
            if status.protocol_version != 0 && status.protocol_version != PROTOCOL_VERSION {
                return Err(BootstrapError::VersionMismatch {
                    client: PROTOCOL_VERSION,
                    server: status.protocol_version,
                });
            }

            let data_socket = kmux_protocol::dirs::data_socket_path()
                .map_err(|e| BootstrapError::ConnectionFailed(format!("data socket path: {e}")))?;

            let stream = tokio::net::UnixStream::connect(&data_socket)
                .await
                .map_err(|e| {
                    BootstrapError::ConnectionFailed(format!(
                        "UDS connect to {}: {e}",
                        data_socket.display()
                    ))
                })?;

            let (mut read_half, mut write_half) = tokio::io::split(stream);

            let (conn_id, server_endpoints) = perform_auth_handshake(
                &mut write_half,
                &mut read_half,
                status.token.clone(),
                self.capabilities.clone(),
                self.connection_id,
                &server_tx,
            )
            .await?;

            info!(
                strategy = "uds-local",
                conn_id = conn_id.0,
                "Bootstrap succeeded"
            );

            let (client_tx, mut client_rx) = mpsc::unbounded_channel::<ClientMessage>();

            let writer_handle = tokio::spawn(async move {
                while let Some(msg) = client_rx.recv().await {
                    match encode_client(&msg) {
                        Ok(bytes) => {
                            if write_frame(&mut write_half, &bytes).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => warn!("UDS bootstrap encode error: {e}"),
                    }
                }
                debug!("UDS bootstrap writer task exited");
            });

            tokio::spawn(async move {
                loop {
                    match read_frame(&mut read_half).await {
                        Ok(Some(data)) => match decode_server(&data) {
                            Ok(msg) => {
                                if server_tx.send(msg).is_err() {
                                    break;
                                }
                            }
                            Err(e) => warn!("UDS bootstrap decode error: {e}"),
                        },
                        Ok(None) => break,
                        Err(e) => {
                            warn!("UDS bootstrap read error: {e}");
                            break;
                        }
                    }
                }
                writer_handle.abort();
                debug!("UDS bootstrap reader task exited");
            });

            Ok(SessionContext {
                token: status.token,
                connection_id: conn_id,
                server_endpoints,
                bootstrap_transport: TransportKind::Uds,
                send: client_tx,
            })
        })
    }
}

// ─── QuicDirectBootstrap ──────────────────────────────────────────────────────

/// Bootstrap via a direct QUIC connection.
///
/// Uses the existing `crate::connect::connect` function internally.
pub struct QuicDirectBootstrap {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
    pub accept_invalid_certs: bool,
}

impl Bootstrap for QuicDirectBootstrap {
    fn name(&self) -> &'static str {
        "quic-direct"
    }

    fn try_bootstrap(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<SessionContext, BootstrapError>> + Send + '_>,
    > {
        Box::pin(async move {
            let result = crate::connect::connect(
                self.host.clone(),
                self.port,
                self.token.clone(),
                self.accept_invalid_certs,
                server_tx,
                self.capabilities.clone(),
                self.connection_id,
            )
            .await;

            match result {
                crate::connect::ConnectResult::Connected(sender) => {
                    // QUIC connect() sends Auth and spawns tasks but doesn't wait
                    // for AuthResult — SessionManager will handle it via server_tx.
                    // We can't extract connection_id here without refactoring connect().
                    // Return a placeholder; SessionManager will update connection_id
                    // via handle_server_message(AuthResult).
                    //
                    // For now, use connection_id = ConnectionId(0) as a sentinel
                    // that will be overwritten by the real AuthResult.
                    info!(
                        strategy = "quic-direct",
                        host = %self.host,
                        port = self.port,
                        "Bootstrap connected (awaiting AuthResult)"
                    );
                    Ok(SessionContext {
                        token: self.token.clone(),
                        connection_id: self.connection_id.unwrap_or(ConnectionId(0)),
                        server_endpoints: Vec::new(),
                        bootstrap_transport: TransportKind::Quic,
                        send: sender,
                    })
                }
                crate::connect::ConnectResult::Failed(e) => {
                    Err(BootstrapError::ConnectionFailed(e))
                }
            }
        })
    }
}

// ─── TlsTcpDirectBootstrap ────────────────────────────────────────────────────

/// Bootstrap via a direct TLS-over-TCP connection.
pub struct TlsTcpDirectBootstrap {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
    pub accept_invalid_certs: bool,
}

impl Bootstrap for TlsTcpDirectBootstrap {
    fn name(&self) -> &'static str {
        "tcp-tls-direct"
    }

    fn try_bootstrap(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<SessionContext, BootstrapError>> + Send + '_>,
    > {
        Box::pin(async move {
            let tofu_key = format!("{}:{}", self.host, self.port);
            let result = crate::tcp_connect::connect_tcp_tls(
                self.host.clone(),
                self.port,
                tofu_key,
                self.token.clone(),
                server_tx,
                self.capabilities.clone(),
                self.connection_id,
                self.accept_invalid_certs,
            )
            .await;

            match result {
                crate::connect::ConnectResult::Connected(sender) => {
                    info!(
                        strategy = "tcp-tls-direct",
                        host = %self.host,
                        port = self.port,
                        "Bootstrap connected (awaiting AuthResult)"
                    );
                    Ok(SessionContext {
                        token: self.token.clone(),
                        connection_id: self.connection_id.unwrap_or(ConnectionId(0)),
                        server_endpoints: Vec::new(),
                        bootstrap_transport: TransportKind::TcpTls,
                        send: sender,
                    })
                }
                crate::connect::ConnectResult::Failed(e) => {
                    Err(BootstrapError::ConnectionFailed(e))
                }
            }
        })
    }
}

// ─── SshBootstrap ─────────────────────────────────────────────────────────────

/// Bootstrap via `kmuxd probe-or-start` over SSH, then TLS-TCP over an SSH `-L`
/// tunnel.
///
/// On success the `SessionContext.send` channel delivers messages over the
/// TLS-TCP tunnel. The caller may later upgrade the data plane to direct QUIC
/// or TLS-TCP when the server becomes reachable directly.
pub struct SshBootstrap {
    pub target: crate::ssh::RemoteTarget,
    pub capabilities: ClientCapabilities,
    pub connection_id: Option<ConnectionId>,
    pub accept_invalid_certs: bool,
}

impl Bootstrap for SshBootstrap {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn try_bootstrap(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<SessionContext, BootstrapError>> + Send + '_>,
    > {
        Box::pin(async move {
            let ssh = crate::ssh::negotiate(&self.target)
                .await
                .map_err(|e| match e {
                    crate::ssh::SshError::DaemonNotInstalled => BootstrapError::RemoteNotInstalled,
                    crate::ssh::SshError::VersionMismatch { client, server } => {
                        BootstrapError::VersionMismatch { client, server }
                    }
                    other => BootstrapError::ConnectionFailed(other.to_string()),
                })?;

            // TOFU key is the *remote* host:port (not the ephemeral local tunnel port).
            let tofu_key = format!("{}:{}", ssh.remote_host, ssh.remote_tcp_port);

            let result = crate::tcp_connect::connect_tcp_tls(
                "127.0.0.1".to_string(),
                ssh.local_tcp_port,
                tofu_key,
                ssh.token.clone(),
                server_tx,
                self.capabilities.clone(),
                self.connection_id,
                self.accept_invalid_certs,
            )
            .await;

            match result {
                crate::connect::ConnectResult::Connected(sender) => {
                    // Keep the SSH tunnel process alive by holding it in a spawned task.
                    let mut tunnel = ssh.tunnel_process;
                    tokio::spawn(async move {
                        let _ = tunnel.wait().await;
                        debug!("SSH tunnel process exited");
                    });

                    info!(
                        strategy = "ssh",
                        remote = %self.target.host,
                        "Bootstrap connected via SSH tunnel (awaiting AuthResult)"
                    );
                    Ok(SessionContext {
                        token: ssh.token,
                        connection_id: self.connection_id.unwrap_or(ConnectionId(0)),
                        server_endpoints: Vec::new(),
                        bootstrap_transport: TransportKind::TcpTls,
                        send: sender,
                    })
                }
                crate::connect::ConnectResult::Failed(e) => Err(BootstrapError::ConnectionFailed(
                    format!("TCP+TLS over SSH tunnel: {e}"),
                )),
            }
        })
    }
}

// ─── bootstrap_race ───────────────────────────────────────────────────────────

/// Run multiple bootstrap strategies concurrently and return the first success.
///
/// Uses `futures::stream::FuturesUnordered` so strategies are polled
/// simultaneously without requiring `'static` bounds. The first
/// `Ok(SessionContext)` wins; all other in-progress strategies are dropped.
///
/// Returns `Err(BootstrapError::AllFailed(_))` with per-strategy error
/// descriptions if every strategy fails.
pub async fn bootstrap_race(
    strategies: Vec<Box<dyn Bootstrap>>,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
) -> Result<SessionContext, BootstrapError> {
    if strategies.is_empty() {
        return Err(BootstrapError::AllFailed(vec![
            "no bootstrap strategies provided".to_string(),
        ]));
    }

    let mut futs: FuturesUnordered<_> = strategies
        .iter()
        .map(|s| {
            let name = s.name();
            let fut = s.try_bootstrap(server_tx.clone());
            async move { (name, fut.await) }
        })
        .collect();

    let mut errors: Vec<String> = Vec::new();

    while let Some((name, result)) = futs.next().await {
        match result {
            Ok(ctx) => {
                info!(strategy = name, "Bootstrap race won");
                return Ok(ctx);
            }
            Err(BootstrapError::NotAvailable) => {
                debug!(
                    strategy = name,
                    "Bootstrap strategy not applicable, skipping"
                );
            }
            // Version mismatch is always fatal — halt immediately with no retry.
            Err(e @ BootstrapError::VersionMismatch { .. }) => {
                warn!(strategy = name, error = %e, "Protocol version mismatch — halting");
                return Err(e);
            }
            Err(e) => {
                warn!(strategy = name, error = %e, "Bootstrap strategy failed");
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    Err(BootstrapError::AllFailed(errors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::TransportKind;
    use tokio::sync::mpsc;

    struct AlwaysFails;
    impl Bootstrap for AlwaysFails {
        fn name(&self) -> &'static str {
            "always-fails"
        }
        fn try_bootstrap(
            &self,
            _server_tx: mpsc::UnboundedSender<ServerMessage>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<SessionContext, BootstrapError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move { Err(BootstrapError::ConnectionFailed("test failure".into())) })
        }
    }

    struct AlwaysNotAvailable;
    impl Bootstrap for AlwaysNotAvailable {
        fn name(&self) -> &'static str {
            "not-available"
        }
        fn try_bootstrap(
            &self,
            _server_tx: mpsc::UnboundedSender<ServerMessage>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<SessionContext, BootstrapError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move { Err(BootstrapError::NotAvailable) })
        }
    }

    struct AlwaysSucceeds {
        kind: TransportKind,
    }
    impl Bootstrap for AlwaysSucceeds {
        fn name(&self) -> &'static str {
            "always-succeeds"
        }
        fn try_bootstrap(
            &self,
            _server_tx: mpsc::UnboundedSender<ServerMessage>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<SessionContext, BootstrapError>>
                    + Send
                    + '_,
            >,
        > {
            let kind = self.kind;
            Box::pin(async move {
                let (tx, _rx) = mpsc::unbounded_channel();
                Ok(SessionContext {
                    token: "tok".into(),
                    connection_id: ConnectionId(42),
                    server_endpoints: Vec::new(),
                    bootstrap_transport: kind,
                    send: tx,
                })
            })
        }
    }

    #[tokio::test]
    async fn bootstrap_race_returns_first_success() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let strategies: Vec<Box<dyn Bootstrap>> = vec![
            Box::new(AlwaysFails),
            Box::new(AlwaysSucceeds {
                kind: TransportKind::Quic,
            }),
        ];
        let ctx = bootstrap_race(strategies, srv_tx).await.unwrap();
        assert_eq!(ctx.connection_id, ConnectionId(42));
        assert_eq!(ctx.bootstrap_transport, TransportKind::Quic);
    }

    #[tokio::test]
    async fn bootstrap_race_all_failed_returns_error() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let strategies: Vec<Box<dyn Bootstrap>> =
            vec![Box::new(AlwaysFails), Box::new(AlwaysFails)];
        let err = bootstrap_race(strategies, srv_tx).await.unwrap_err();
        assert!(matches!(err, BootstrapError::AllFailed(_)));
    }

    #[tokio::test]
    async fn bootstrap_race_not_available_skipped_and_success_wins() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let strategies: Vec<Box<dyn Bootstrap>> = vec![
            Box::new(AlwaysNotAvailable),
            Box::new(AlwaysSucceeds {
                kind: TransportKind::Uds,
            }),
        ];
        let ctx = bootstrap_race(strategies, srv_tx).await.unwrap();
        assert_eq!(ctx.bootstrap_transport, TransportKind::Uds);
    }

    #[tokio::test]
    async fn bootstrap_race_empty_strategies_fails() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let err = bootstrap_race(vec![], srv_tx).await.unwrap_err();
        assert!(matches!(err, BootstrapError::AllFailed(_)));
    }

    #[tokio::test]
    async fn bootstrap_race_not_available_only_returns_all_failed() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let strategies: Vec<Box<dyn Bootstrap>> =
            vec![Box::new(AlwaysNotAvailable), Box::new(AlwaysNotAvailable)];
        // NotAvailable is silently skipped; with no successes and no real errors,
        // we still return AllFailed (with an empty error list).
        let err = bootstrap_race(strategies, srv_tx).await.unwrap_err();
        assert!(matches!(err, BootstrapError::AllFailed(_)));
    }

    struct VersionMismatchStrategy;
    impl Bootstrap for VersionMismatchStrategy {
        fn name(&self) -> &'static str {
            "version-mismatch"
        }
        fn try_bootstrap(
            &self,
            _server_tx: mpsc::UnboundedSender<ServerMessage>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<SessionContext, BootstrapError>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async move {
                Err(BootstrapError::VersionMismatch {
                    client: 13,
                    server: 99,
                })
            })
        }
    }

    #[tokio::test]
    async fn bootstrap_race_version_mismatch_halts_immediately() {
        let (srv_tx, _srv_rx) = mpsc::unbounded_channel();
        let strategies: Vec<Box<dyn Bootstrap>> = vec![
            // Version mismatch must halt even if another strategy would succeed.
            Box::new(VersionMismatchStrategy),
            Box::new(AlwaysSucceeds {
                kind: TransportKind::Quic,
            }),
        ];
        let err = bootstrap_race(strategies, srv_tx).await.unwrap_err();
        assert!(
            matches!(
                err,
                BootstrapError::VersionMismatch {
                    client: 13,
                    server: 99
                }
            ),
            "expected VersionMismatch, got: {err:?}"
        );
    }
}
