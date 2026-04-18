//! One-shot bootstrap pipeline: the single code path for establishing a
//! kmux data connection.
//!
//! Used by the TUI (with a [`NoopObserver`]) and by `--dry-run` / `--test`
//! (with a `ConsoleObserver` in the `kmux` crate). Both callers run the
//! same `run_bootstrap` so diagnostics exercise the real flow instead of
//! a lookalike.

use std::path::Path;
use std::time::{Duration, Instant};

use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, ConnectionId, PROTOCOL_VERSION, ServerMessage,
};
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::connect::{self, ConnectResult};
use crate::daemon::{self, ensure_daemon};
use crate::ssh::{self, RemoteTarget, SshError};
use crate::tcp_connect;
use crate::transport::TransportKind;

/// How long to wait for `AuthResult` after sending `Auth` on a transport.
/// Must be long enough for SSH + TLS handshake + first frame round-trip
/// on a slow link, but short enough to fail visibly rather than hang.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

// ─── Public types ─────────────────────────────────────────────────────────

/// Target to bootstrap against, resolved from CLI arguments but without
/// any network or filesystem I/O having been performed yet. All I/O
/// happens inside `run_bootstrap`.
#[derive(Debug, Clone)]
pub enum ResolvedTarget {
    /// Connect to the local daemon. Starts one on demand if not running.
    LocalDaemon,
    /// Direct transport to `host:port` with an explicit token (no SSH).
    Direct {
        host: String,
        port: u16,
        token: String,
        accept_invalid_certs: bool,
    },
    /// SSH negotiation, then TLS-TCP over the resulting `-L` tunnel.
    Ssh {
        target: RemoteTarget,
        accept_invalid_certs: bool,
    },
}

impl ResolvedTarget {
    pub fn label(&self) -> String {
        match self {
            ResolvedTarget::LocalDaemon => "local-daemon".to_string(),
            ResolvedTarget::Direct { host, port, .. } => format!("direct {host}:{port}"),
            ResolvedTarget::Ssh { target, .. } => match &target.user {
                Some(u) => format!("ssh {u}@{}", target.host),
                None => format!("ssh {}", target.host),
            },
        }
    }
}

/// SSH-specific state carried out of a successful bootstrap. Callers
/// that drive a live TUI use this to spawn a tunnel-death monitor plus
/// a [`crate::supervisor::TransportSupervisor`] that probes for a
/// direct-QUIC upgrade.
pub struct SshContext {
    pub tunnel_process: tokio::process::Child,
    pub remote_host: String,
    pub quic_port: u16,
    pub remote_tcp_port: u16,
    /// Upgrade candidates for the supervisor (direct QUIC on the remote).
    pub endpoints: Vec<EndpointAdvert>,
    /// Raw probe-or-start JSON, kept for diagnostic output.
    pub probe_json: String,
}

/// Outcome of a successful [`run_bootstrap`].
pub struct BootstrapOutcome {
    pub client_tx: mpsc::UnboundedSender<ClientMessage>,
    pub transport: TransportKind,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub capabilities: ClientCapabilities,
    pub accept_invalid_certs: bool,
    pub connection_id: ConnectionId,
    pub server_version: Option<String>,
    pub is_local: bool,
    /// `Some` for SSH targets; `None` for LocalDaemon / Direct.
    pub ssh_context: Option<SshContext>,
    pub bootstrap_elapsed: Duration,
}

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("daemon start failed: {0}")]
    DaemonStart(String),
    #[error("SSH negotiation failed: {0}")]
    Ssh(#[from] SshError),
    #[error("{strategy} connect failed: {error}")]
    Connect {
        strategy: &'static str,
        error: String,
    },
    #[error("auth rejected: {0}")]
    Auth(String),
    #[error("auth timed out after {0:?}")]
    AuthTimeout(Duration),
    #[error("server sent success=true but no connection_id")]
    MissingConnectionId,
}

// ─── Observer ─────────────────────────────────────────────────────────────

/// Diagnostic events emitted by [`run_bootstrap`]. The `NoopObserver`
/// forwards each to `tracing::debug!`; the console observer used by
/// `--dry-run` prints them as human-readable lines on stdout.
///
/// `#[non_exhaustive]` so new steps can be added without breaking
/// downstream observer implementations.
#[non_exhaustive]
pub enum BootstrapEvent<'a> {
    ParsedTarget {
        target: &'a ResolvedTarget,
    },

    DaemonQuery {
        socket: &'a Path,
    },
    DaemonAlreadyRunning {
        pid: u32,
        port: u16,
        tcp_port: u16,
    },
    DaemonNotRunning,
    DaemonSpawning {
        binary: &'a Path,
    },
    DaemonReady {
        pid: u32,
        port: u16,
        tcp_port: u16,
        elapsed: Duration,
    },

    SshProbeStarting {
        dest: &'a str,
    },
    /// Raw JSON from `kmuxd probe-or-start`. Observers are expected to
    /// redact the `token` field before printing.
    SshProbeResponseRaw {
        json: &'a str,
    },
    SshProtocolVersionOk {
        version: u32,
    },
    SshTunnelReady {
        local_port: u16,
        remote_port: u16,
        elapsed: Duration,
    },

    HandshakeStarting {
        transport: TransportKind,
        host: &'a str,
        port: u16,
    },
    HandshakeAuthSent {
        protocol_version: u32,
        connection_id: Option<ConnectionId>,
    },
    HandshakeAuthResult {
        success: bool,
        connection_id: Option<ConnectionId>,
        server_version: Option<&'a str>,
        reason: Option<&'a str>,
    },

    BootstrapFailure {
        strategy: &'static str,
        error: &'a BootstrapError,
    },
}

pub trait BootstrapObserver: Send + Sync {
    fn on_event(&self, event: &BootstrapEvent<'_>);
}

/// Default observer for the TUI path: forwards to `tracing::debug!` so
/// the existing log file remains informative without adding new lines
/// to stdout.
pub struct NoopObserver;

impl BootstrapObserver for NoopObserver {
    fn on_event(&self, event: &BootstrapEvent<'_>) {
        match event {
            BootstrapEvent::ParsedTarget { target } => {
                debug!(target = %target.label(), "pipeline: parsed target");
            }
            BootstrapEvent::DaemonQuery { socket } => {
                debug!(socket = %socket.display(), "pipeline: querying daemon");
            }
            BootstrapEvent::DaemonAlreadyRunning {
                pid,
                port,
                tcp_port,
            } => {
                debug!(pid, port, tcp_port, "pipeline: daemon already running");
            }
            BootstrapEvent::DaemonNotRunning => debug!("pipeline: daemon not running"),
            BootstrapEvent::DaemonSpawning { binary } => {
                debug!(binary = %binary.display(), "pipeline: spawning daemon");
            }
            BootstrapEvent::DaemonReady {
                pid,
                port,
                tcp_port,
                elapsed,
            } => {
                debug!(pid, port, tcp_port, ?elapsed, "pipeline: daemon ready");
            }
            BootstrapEvent::SshProbeStarting { dest } => {
                debug!(dest, "pipeline: ssh probe-or-start");
            }
            BootstrapEvent::SshProbeResponseRaw { .. } => {
                debug!("pipeline: ssh probe response received");
            }
            BootstrapEvent::SshProtocolVersionOk { version } => {
                debug!(version, "pipeline: ssh protocol version ok");
            }
            BootstrapEvent::SshTunnelReady {
                local_port,
                remote_port,
                elapsed,
            } => {
                debug!(
                    local_port,
                    remote_port,
                    ?elapsed,
                    "pipeline: ssh tunnel ready"
                );
            }
            BootstrapEvent::HandshakeStarting {
                transport,
                host,
                port,
            } => {
                debug!(transport = %transport, host, port, "pipeline: handshake starting");
            }
            BootstrapEvent::HandshakeAuthSent {
                protocol_version,
                connection_id,
            } => {
                debug!(
                    protocol_version,
                    connection_id = connection_id.map(|c| c.0),
                    "pipeline: auth sent",
                );
            }
            BootstrapEvent::HandshakeAuthResult {
                success,
                connection_id,
                server_version,
                reason,
            } => {
                debug!(
                    success,
                    connection_id = connection_id.map(|c| c.0),
                    server_version = server_version.as_deref(),
                    reason = reason.as_deref(),
                    "pipeline: auth result",
                );
            }
            BootstrapEvent::BootstrapFailure { strategy, error } => {
                warn!(strategy, error = %error, "pipeline: bootstrap failed");
            }
        }
    }
}

// ─── run_bootstrap ────────────────────────────────────────────────────────

struct ConnectPlan {
    transport: TransportKind,
    host: String,
    port: u16,
    token: String,
    accept_invalid_certs: bool,
    is_local: bool,
    ssh_context: Option<SshContext>,
}

/// Bootstrap `target`, emitting one [`BootstrapEvent`] per step through
/// `observer`. Returns only after `AuthResult { success: true }` has
/// been observed on the data-plane socket, so the returned
/// [`BootstrapOutcome::client_tx`] is usable immediately.
///
/// The caller's `server_tx` receives every server message (including the
/// captured `AuthResult`), so downstream state machines that expect to
/// process `AuthResult` via their message pump continue to work.
pub async fn run_bootstrap(
    target: ResolvedTarget,
    capabilities: ClientCapabilities,
    connection_id: Option<ConnectionId>,
    server_tx: mpsc::UnboundedSender<ServerMessage>,
    observer: &dyn BootstrapObserver,
) -> Result<BootstrapOutcome, BootstrapError> {
    let start = Instant::now();
    observer.on_event(&BootstrapEvent::ParsedTarget { target: &target });

    let plan = match target {
        ResolvedTarget::LocalDaemon => prepare_local_daemon(observer).await?,
        ResolvedTarget::Direct {
            host,
            port,
            token,
            accept_invalid_certs,
        } => ConnectPlan {
            transport: TransportKind::Quic,
            host,
            port,
            token,
            accept_invalid_certs,
            is_local: false,
            ssh_context: None,
        },
        ResolvedTarget::Ssh {
            target,
            accept_invalid_certs,
        } => prepare_ssh(target, accept_invalid_certs, observer).await?,
    };

    let (client_tx, auth) = establish(
        &plan,
        connection_id,
        capabilities.clone(),
        server_tx,
        observer,
    )
    .await?;

    Ok(BootstrapOutcome {
        client_tx,
        transport: plan.transport,
        host: plan.host,
        port: plan.port,
        token: plan.token,
        capabilities,
        accept_invalid_certs: plan.accept_invalid_certs,
        connection_id: auth.connection_id,
        server_version: auth.server_version,
        is_local: plan.is_local,
        ssh_context: plan.ssh_context,
        bootstrap_elapsed: start.elapsed(),
    })
}

async fn prepare_local_daemon(
    observer: &dyn BootstrapObserver,
) -> Result<ConnectPlan, BootstrapError> {
    let socket = kmux_protocol::dirs::socket_path()
        .map_err(|e| BootstrapError::DaemonStart(format!("socket path: {e}")))?;
    observer.on_event(&BootstrapEvent::DaemonQuery { socket: &socket });

    let existing = daemon::query_daemon().await;
    let start = Instant::now();
    if let Some(status) = &existing {
        observer.on_event(&BootstrapEvent::DaemonAlreadyRunning {
            pid: status.pid,
            port: status.port,
            tcp_port: status.tcp_port,
        });
    } else {
        observer.on_event(&BootstrapEvent::DaemonNotRunning);
        if let Ok(binary) = daemon::find_server_binary() {
            observer.on_event(&BootstrapEvent::DaemonSpawning { binary: &binary });
        }
    }

    let status = ensure_daemon()
        .await
        .map_err(|e| BootstrapError::DaemonStart(e.to_string()))?;

    if existing.is_none() {
        observer.on_event(&BootstrapEvent::DaemonReady {
            pid: status.pid,
            port: status.port,
            tcp_port: status.tcp_port,
            elapsed: start.elapsed(),
        });
    }

    Ok(ConnectPlan {
        transport: TransportKind::Quic,
        host: "127.0.0.1".to_string(),
        port: status.port,
        token: status.token,
        accept_invalid_certs: true,
        is_local: true,
        ssh_context: None,
    })
}

async fn prepare_ssh(
    target: RemoteTarget,
    accept_invalid: bool,
    observer: &dyn BootstrapObserver,
) -> Result<ConnectPlan, BootstrapError> {
    let dest = match &target.user {
        Some(u) => format!("{u}@{}", target.host),
        None => target.host.clone(),
    };
    observer.on_event(&BootstrapEvent::SshProbeStarting { dest: &dest });

    let start = Instant::now();
    let ssh = ssh::negotiate(&target).await?;

    observer.on_event(&BootstrapEvent::SshProbeResponseRaw {
        json: &ssh.probe_json,
    });
    observer.on_event(&BootstrapEvent::SshProtocolVersionOk {
        version: PROTOCOL_VERSION,
    });
    observer.on_event(&BootstrapEvent::SshTunnelReady {
        local_port: ssh.local_tcp_port,
        remote_port: ssh.remote_tcp_port,
        elapsed: start.elapsed(),
    });

    let endpoints = vec![EndpointAdvert {
        kind: TransportKind::Quic,
        address: format!("{}:{}", ssh.remote_host, ssh.quic_port),
    }];

    Ok(ConnectPlan {
        transport: TransportKind::TcpTls,
        host: "127.0.0.1".to_string(),
        port: ssh.local_tcp_port,
        token: ssh.token.clone(),
        accept_invalid_certs: accept_invalid,
        is_local: false,
        ssh_context: Some(SshContext {
            tunnel_process: ssh.tunnel_process,
            remote_host: ssh.remote_host,
            quic_port: ssh.quic_port,
            remote_tcp_port: ssh.remote_tcp_port,
            endpoints,
            probe_json: ssh.probe_json,
        }),
    })
}

struct AuthOutcome {
    connection_id: ConnectionId,
    server_version: Option<String>,
}

/// Open the data-plane connection for `plan`, capturing `AuthResult`
/// via an intercept task so it arrives at the caller's `server_tx` and
/// is surfaced synchronously to `run_bootstrap`.
async fn establish(
    plan: &ConnectPlan,
    connection_id: Option<ConnectionId>,
    capabilities: ClientCapabilities,
    outer_tx: mpsc::UnboundedSender<ServerMessage>,
    observer: &dyn BootstrapObserver,
) -> Result<(mpsc::UnboundedSender<ClientMessage>, AuthOutcome), BootstrapError> {
    observer.on_event(&BootstrapEvent::HandshakeStarting {
        transport: plan.transport,
        host: &plan.host,
        port: plan.port,
    });

    let (intercept_tx, mut intercept_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (auth_tx, auth_rx) = oneshot::channel::<Result<AuthOutcome, String>>();

    let forwarder_outer = outer_tx.clone();
    tokio::spawn(async move {
        let mut auth_tx = Some(auth_tx);
        while let Some(msg) = intercept_rx.recv().await {
            if let (Some(_), ServerMessage::AuthResult { .. }) = (&auth_tx, &msg) {
                let captured = match &msg {
                    ServerMessage::AuthResult {
                        success: true,
                        connection_id,
                        server_version,
                        ..
                    } => Ok(AuthOutcome {
                        connection_id: connection_id.unwrap_or(ConnectionId(0)),
                        server_version: server_version.clone(),
                    }),
                    ServerMessage::AuthResult {
                        success: false,
                        reason,
                        ..
                    } => Err(reason.clone().unwrap_or_else(|| "rejected".into())),
                    _ => unreachable!(),
                };
                if let Some(tx) = auth_tx.take() {
                    let _ = tx.send(captured);
                }
            }
            if forwarder_outer.send(msg).is_err() {
                break;
            }
        }
    });

    let sender_result = match plan.transport {
        TransportKind::Quic => {
            connect::connect(
                plan.host.clone(),
                plan.port,
                plan.token.clone(),
                plan.accept_invalid_certs,
                intercept_tx,
                capabilities,
                connection_id,
            )
            .await
        }
        TransportKind::TcpTls => {
            let tofu_key = match &plan.ssh_context {
                Some(ctx) => format!("{}:{}", ctx.remote_host, ctx.remote_tcp_port),
                None => format!("{}:{}", plan.host, plan.port),
            };
            tcp_connect::connect_tcp_tls(
                plan.host.clone(),
                plan.port,
                tofu_key,
                plan.token.clone(),
                intercept_tx,
                capabilities,
                connection_id,
                plan.accept_invalid_certs,
            )
            .await
        }
        TransportKind::Uds => {
            let socket_path =
                kmux_protocol::dirs::data_socket_path().map_err(|e| BootstrapError::Connect {
                    strategy: "uds",
                    error: format!("data socket path: {e}"),
                })?;
            tcp_connect::connect_uds(
                socket_path,
                plan.token.clone(),
                intercept_tx,
                capabilities,
                connection_id,
            )
            .await
        }
        TransportKind::Tcp => {
            return Err(BootstrapError::Connect {
                strategy: "tcp",
                error: "plain TCP bootstrap is not supported; \
                        use quic, tcp+tls, uds, or ssh"
                    .into(),
            });
        }
    };

    observer.on_event(&BootstrapEvent::HandshakeAuthSent {
        protocol_version: PROTOCOL_VERSION,
        connection_id,
    });

    let client_tx = match sender_result {
        ConnectResult::Connected(tx) => tx,
        ConnectResult::Failed(e) => {
            let err = BootstrapError::Connect {
                strategy: transport_strategy_name(plan.transport),
                error: e,
            };
            observer.on_event(&BootstrapEvent::BootstrapFailure {
                strategy: transport_strategy_name(plan.transport),
                error: &err,
            });
            return Err(err);
        }
    };

    match timeout(AUTH_TIMEOUT, auth_rx).await {
        Ok(Ok(Ok(auth))) => {
            observer.on_event(&BootstrapEvent::HandshakeAuthResult {
                success: true,
                connection_id: Some(auth.connection_id),
                server_version: auth.server_version.as_deref(),
                reason: None,
            });
            if auth.connection_id == ConnectionId(0) {
                return Err(BootstrapError::MissingConnectionId);
            }
            Ok((client_tx, auth))
        }
        Ok(Ok(Err(reason))) => {
            observer.on_event(&BootstrapEvent::HandshakeAuthResult {
                success: false,
                connection_id: None,
                server_version: None,
                reason: Some(&reason),
            });
            Err(BootstrapError::Auth(reason))
        }
        Ok(Err(_)) => Err(BootstrapError::Auth(
            "auth forwarder dropped before AuthResult".into(),
        )),
        Err(_) => Err(BootstrapError::AuthTimeout(AUTH_TIMEOUT)),
    }
}

fn transport_strategy_name(t: TransportKind) -> &'static str {
    match t {
        TransportKind::Quic => "quic",
        TransportKind::TcpTls => "tcp+tls",
        TransportKind::Uds => "uds",
        TransportKind::Tcp => "tcp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_target_label_formats_each_variant() {
        assert_eq!(ResolvedTarget::LocalDaemon.label(), "local-daemon");

        let d = ResolvedTarget::Direct {
            host: "example.com".into(),
            port: 8443,
            token: "t".into(),
            accept_invalid_certs: false,
        };
        assert_eq!(d.label(), "direct example.com:8443");

        let s = ResolvedTarget::Ssh {
            target: RemoteTarget {
                user: Some("alice".into()),
                host: "srv".into(),
                ssh_port: None,
            },
            accept_invalid_certs: false,
        };
        assert_eq!(s.label(), "ssh alice@srv");
    }

    #[test]
    fn transport_strategy_name_covers_all_kinds() {
        assert_eq!(transport_strategy_name(TransportKind::Quic), "quic");
        assert_eq!(transport_strategy_name(TransportKind::TcpTls), "tcp+tls");
        assert_eq!(transport_strategy_name(TransportKind::Uds), "uds");
        assert_eq!(transport_strategy_name(TransportKind::Tcp), "tcp");
    }

    #[test]
    fn noop_observer_accepts_every_event() {
        // The observer is infallible; this just checks that we constructed
        // every variant successfully (catches any new non_exhaustive arm
        // that was added without a corresponding NoopObserver handler).
        let target = ResolvedTarget::LocalDaemon;
        let o = NoopObserver;
        o.on_event(&BootstrapEvent::ParsedTarget { target: &target });
        o.on_event(&BootstrapEvent::DaemonNotRunning);
        o.on_event(&BootstrapEvent::SshProtocolVersionOk { version: 13 });
        o.on_event(&BootstrapEvent::HandshakeAuthSent {
            protocol_version: 13,
            connection_id: Some(ConnectionId(7)),
        });
        o.on_event(&BootstrapEvent::HandshakeAuthResult {
            success: true,
            connection_id: Some(ConnectionId(7)),
            server_version: Some("1.2.3"),
            reason: None,
        });
    }
}
