//! Single reconnect entry point.
//!
//! This module exists so the TUI event loop does not build bootstrap
//! strategies itself — that logic would then duplicate between initial
//! connect and reconnect. [`ReconnectContext`] captures everything needed
//! to re-run [`bootstrap_race`] and is the only way anything in the app
//! triggers a reconnect.
//!
//! The module deliberately does **not** own transport-supervisor setup or
//! tunnel-death monitoring; those are per-frontend concerns and live in
//! `kmux/src/app/helpers.rs`. Recovery stops at "we have a new
//! authenticated data-plane sender."

use kmux_protocol::messages::{ClientCapabilities, ConnectionId, ServerMessage};
use kmux_protocol::transport::bootstrap::{Bootstrap, BootstrapError, SessionContext};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[cfg(feature = "remote")]
use crate::bootstrap::{QuicDirectBootstrap, SshBootstrap, TlsTcpDirectBootstrap};
use crate::bootstrap::{UdsLocalBootstrap, bootstrap_race};
use crate::ssh::RemoteTarget;

/// Everything needed to (re-)run `bootstrap_race`. Populated once from CLI
/// args / server picker input and persisted for the lifetime of the
/// frontend.
#[derive(Debug, Clone)]
pub struct ReconnectContext {
    /// Client capability negotiation payload.
    pub capabilities: ClientCapabilities,
    pub accept_invalid_certs: bool,

    /// Direct host/port for QUIC + TLS-TCP strategies. Ignored when
    /// `ssh_target` is set and `host` is empty.
    pub host: String,
    pub port: u16,
    pub token: String,

    /// Present when the user originally connected via SSH. Enables the
    /// `SshBootstrap` strategy (which runs `kmuxd probe-or-start` and
    /// will start the remote daemon if it is not running).
    pub ssh_target: Option<RemoteTarget>,

    /// True when targeting the local daemon (enables the UDS strategy).
    pub is_local: bool,
}

impl ReconnectContext {
    /// Build the ordered set of bootstrap strategies for the current
    /// target. `connection_id` is threaded through so the server can
    /// re-attach pane streams to the new data-plane channel.
    pub fn build_strategies(&self, connection_id: Option<ConnectionId>) -> Vec<Box<dyn Bootstrap>> {
        let mut strategies: Vec<Box<dyn Bootstrap>> = Vec::new();

        if self.is_local {
            strategies.push(Box::new(UdsLocalBootstrap {
                capabilities: self.capabilities.clone(),
                connection_id,
            }));
        }

        #[cfg(feature = "remote")]
        if !self.host.is_empty() && self.port != 0 && !self.token.is_empty() {
            strategies.push(Box::new(QuicDirectBootstrap {
                host: self.host.clone(),
                port: self.port,
                token: self.token.clone(),
                capabilities: self.capabilities.clone(),
                connection_id,
                accept_invalid_certs: self.accept_invalid_certs,
            }));
            strategies.push(Box::new(TlsTcpDirectBootstrap {
                host: self.host.clone(),
                port: self.port,
                token: self.token.clone(),
                capabilities: self.capabilities.clone(),
                connection_id,
                accept_invalid_certs: self.accept_invalid_certs,
            }));
        }

        #[cfg(feature = "remote")]
        if let Some(target) = &self.ssh_target {
            strategies.push(Box::new(SshBootstrap {
                target: target.clone(),
                capabilities: self.capabilities.clone(),
                connection_id,
                accept_invalid_certs: self.accept_invalid_certs,
            }));
        }

        strategies
    }

    /// Run `bootstrap_race` using the strategies implied by this context.
    /// Returns the first successful `SessionContext`.
    pub async fn run(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
        connection_id: Option<ConnectionId>,
    ) -> Result<SessionContext, BootstrapError> {
        let strategies = self.build_strategies(connection_id);
        if strategies.is_empty() {
            warn!("No reconnect strategies available");
            return Err(BootstrapError::AllFailed(vec![
                "no reconnect strategies (missing host/token/ssh target)".to_string(),
            ]));
        }
        info!(
            count = strategies.len(),
            connection_id = connection_id.map(|c| c.0),
            "Running bootstrap_race for reconnect"
        );
        bootstrap_race(strategies, server_tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> ClientCapabilities {
        ClientCapabilities::default()
    }

    #[test]
    fn local_context_yields_uds_strategy() {
        let ctx = ReconnectContext {
            capabilities: caps(),
            accept_invalid_certs: false,
            host: String::new(),
            port: 0,
            token: String::new(),
            ssh_target: None,
            is_local: true,
        };
        let strategies = ctx.build_strategies(None);
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies[0].name(), "uds-local");
    }

    #[cfg(feature = "remote")]
    #[test]
    fn direct_context_yields_quic_and_tls_tcp() {
        let ctx = ReconnectContext {
            capabilities: caps(),
            accept_invalid_certs: false,
            host: "example.com".into(),
            port: 8443,
            token: "tok".into(),
            ssh_target: None,
            is_local: false,
        };
        let strategies = ctx.build_strategies(Some(ConnectionId(7)));
        assert_eq!(strategies.len(), 2);
        assert_eq!(strategies[0].name(), "quic-direct");
        assert_eq!(strategies[1].name(), "tcp-tls-direct");
    }

    #[cfg(feature = "remote")]
    #[test]
    fn ssh_context_includes_ssh_strategy() {
        let ctx = ReconnectContext {
            capabilities: caps(),
            accept_invalid_certs: false,
            host: String::new(),
            port: 0,
            token: String::new(),
            ssh_target: Some(RemoteTarget {
                user: Some("alice".into()),
                host: "example.com".into(),
                ssh_port: None,
            }),
            is_local: false,
        };
        let strategies = ctx.build_strategies(None);
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies[0].name(), "ssh");
    }

    #[cfg(feature = "remote")]
    #[test]
    fn mixed_context_combines_all_applicable_strategies() {
        let ctx = ReconnectContext {
            capabilities: caps(),
            accept_invalid_certs: true,
            host: "ex.com".into(),
            port: 9000,
            token: "tok".into(),
            ssh_target: Some(RemoteTarget {
                user: None,
                host: "ex.com".into(),
                ssh_port: Some(22),
            }),
            is_local: true,
        };
        let names: Vec<_> = ctx
            .build_strategies(None)
            .iter()
            .map(|s| s.name())
            .collect();
        assert_eq!(names, ["uds-local", "quic-direct", "tcp-tls-direct", "ssh"]);
    }

    #[tokio::test]
    async fn run_fails_cleanly_with_no_strategies() {
        let ctx = ReconnectContext {
            capabilities: caps(),
            accept_invalid_certs: false,
            host: String::new(),
            port: 0,
            token: String::new(),
            ssh_target: None,
            is_local: false,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let err = ctx.run(tx, None).await.unwrap_err();
        assert!(matches!(err, BootstrapError::AllFailed(_)));
    }
}
