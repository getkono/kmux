mod daemon_cmd;
mod dry_run;
mod list;
pub mod render;
pub use daemon_cmd::run_daemon_command;
pub use dry_run::run_dry_run;
pub use list::{ListSessionsConfig, run_list_sessions};

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::ssh::{self, ParsedServer};

use crate::cli::ResolvedConnection;

/// Resolve CLI args to a [`ResolvedTarget`] without any network I/O.
///
/// `server == None` selects the local daemon (UDS). Any non-empty string
/// resolves to an SSH target — there is no direct-QUIC CLI surface. All I/O
/// (daemon probe, SSH negotiation, handshake) happens inside
/// [`kmux_client::pipeline::run_bootstrap`].
pub fn parse_target(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
) -> (ResolvedTarget, Option<ParsedServer>) {
    let parsed = server.map(ssh::parse_server_string);

    let target = match parsed.as_ref().and_then(ssh::resolve_remote_target) {
        Some(mut t) => {
            if let Some(p) = ssh_port_override {
                t.ssh_port = Some(p);
            }
            ResolvedTarget::Ssh {
                target: t,
                accept_invalid_certs: true,
            }
        }
        None => ResolvedTarget::LocalDaemon,
    };

    (target, parsed)
}

/// Resolve connection parameters for headless subcommands (`list-sessions`).
///
/// Two modes: local daemon (UDS-mediated TCP token) or SSH (full negotiation
/// plus tunnel). Unlike the TUI path, this performs negotiation up front
/// because list.rs speaks raw TCP rather than going through the bootstrap
/// pipeline.
pub async fn resolve_connection(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
) -> anyhow::Result<ResolvedConnection> {
    let parsed = server.map(ssh::parse_server_string);
    let ssh_target = parsed
        .as_ref()
        .and_then(ssh::resolve_remote_target)
        .map(|mut t| {
            if let Some(p) = ssh_port_override {
                t.ssh_port = Some(p);
            }
            t
        });

    if let Some(target) = ssh_target {
        tracing::info!(
            host = %target.host,
            user = ?target.user,
            "SSH negotiation starting"
        );
        let session = ssh::negotiate(&target)
            .await
            .map_err(|e| anyhow::anyhow!("SSH negotiation failed: {e}"))?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: session.local_tcp_port,
            tcp_port: None,
            token: session.token,
        })
    } else {
        let status = kmux_client::daemon::ensure_compatible_daemon().await?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: status.port,
            tcp_port: Some(status.tcp_port),
            token: status.token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_is_local_daemon() {
        let (target, parsed) = parse_target(None, None);
        assert!(matches!(target, ResolvedTarget::LocalDaemon));
        assert!(parsed.is_none());
    }

    #[test]
    fn bare_hostname_is_ssh() {
        // The historical bug: `kmux focalors` parsed to a Direct target with
        // an empty token, dropping the user into the unrecoverable Connect
        // form. After the SSH-strict change every non-empty server string
        // resolves to an Ssh target.
        let (target, _) = parse_target(Some("focalors"), None);
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.host, "focalors");
                assert!(target.ssh_port.is_none());
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }

    #[test]
    fn host_colon_port_is_ssh_port() {
        // `host:port` (no `@`) is the SSH port, not a daemon data-plane port.
        let (target, _) = parse_target(Some("focalors:2222"), None);
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.host, "focalors");
                assert_eq!(target.ssh_port, Some(2222));
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }

    #[test]
    fn ssh_port_flag_overrides_server_string() {
        let (target, _) = parse_target(Some("focalors:2222"), Some(2223));
        match target {
            ResolvedTarget::Ssh { target, .. } => {
                assert_eq!(target.ssh_port, Some(2223));
            }
            other => panic!("expected Ssh target, got {other:?}"),
        }
    }
}
