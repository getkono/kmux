mod daemon_cmd;
mod dry_run;
mod list;
pub mod render;
pub use daemon_cmd::run_daemon_command;
pub use dry_run::run_dry_run;
pub use list::{ListSessionsConfig, run_list_sessions};

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::ssh::{self, ParsedServer};
use kmux_client::token::read_local_token;

use crate::cli::ResolvedConnection;

/// Resolve CLI args to a [`ResolvedTarget`] without any network I/O.
///
/// All I/O (daemon probe, SSH negotiation, TLS/QUIC handshake) happens
/// inside [`kmux_client::pipeline::run_bootstrap`]. This function is a
/// pure parser so the resulting target can be handed to either the TUI
/// or `--dry-run` without divergence.
pub fn parse_target(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
    no_ssh: bool,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    accept_invalid_certs: bool,
) -> (ResolvedTarget, Option<ParsedServer>) {
    let is_local = server.is_none()
        && host_override.is_none()
        && port_override.is_none()
        && token_override.is_none();

    let parsed = server.map(ssh::parse_server_string);

    let ssh_target = if !no_ssh {
        parsed
            .as_ref()
            .and_then(ssh::resolve_remote_target)
            .map(|mut t| {
                if let Some(p) = ssh_port_override {
                    t.ssh_port = Some(p);
                }
                t
            })
    } else {
        None
    };

    let target = if let Some(target) = ssh_target {
        ResolvedTarget::Ssh {
            target,
            accept_invalid_certs: true,
        }
    } else if is_local {
        ResolvedTarget::LocalDaemon
    } else {
        let (host, port) = if let Some(ref p) = parsed {
            (p.host.clone(), port_override.or(p.port).unwrap_or(8443))
        } else {
            (
                host_override
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "127.0.0.1".to_string()),
                port_override.unwrap_or(8443),
            )
        };
        let token = token_override
            .map(|s| s.to_string())
            .or_else(read_local_token)
            .unwrap_or_default();
        ResolvedTarget::Direct {
            host,
            port,
            token,
            accept_invalid_certs,
        }
    };

    (target, parsed)
}

/// Resolve connection parameters for headless subcommands (`list-sessions`).
///
/// Handles three modes: local daemon, SSH negotiation, or direct. Unlike the
/// TUI path (which uses `parse_target` and defers all I/O to the pipeline),
/// this performs SSH negotiation / daemon ensure up front because `list.rs`
/// speaks raw TCP rather than going through [`kmux_client::pipeline`].
pub async fn resolve_connection(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
    no_ssh: bool,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    _accept_invalid_certs: bool,
) -> anyhow::Result<ResolvedConnection> {
    let is_local = server.is_none()
        && host_override.is_none()
        && port_override.is_none()
        && token_override.is_none();

    let parsed = server.map(ssh::parse_server_string);

    let ssh_target = if !no_ssh {
        parsed
            .as_ref()
            .and_then(ssh::resolve_remote_target)
            .map(|mut t| {
                if let Some(p) = ssh_port_override {
                    t.ssh_port = Some(p);
                }
                t
            })
    } else {
        None
    };

    if let Some(target) = ssh_target {
        tracing::info!(
            host = %target.host,
            user = ?target.user,
            "SSH negotiation starting"
        );
        match ssh::negotiate(&target).await {
            Ok(session) => Ok(ResolvedConnection {
                host: "127.0.0.1".to_string(),
                port: session.local_tcp_port,
                tcp_port: None,
                token: session.token,
            }),
            Err(e) => Err(anyhow::anyhow!("SSH negotiation failed: {e}")),
        }
    } else if is_local {
        let status = kmux_client::daemon::ensure_compatible_daemon().await?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: status.port,
            tcp_port: Some(status.tcp_port),
            token: status.token,
        })
    } else {
        let (host, port) = if let Some(ref parsed) = parsed {
            (
                parsed.host.clone(),
                port_override.or(parsed.port).unwrap_or(8443),
            )
        } else {
            let host = host_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = port_override.unwrap_or(8443);
            (host, port)
        };
        let token = token_override
            .map(|s| s.to_string())
            .or_else(read_local_token)
            .unwrap_or_default();
        Ok(ResolvedConnection {
            host,
            port,
            tcp_port: None,
            token,
        })
    }
}
