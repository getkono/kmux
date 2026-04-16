mod daemon_cmd;
mod list;
pub use daemon_cmd::run_daemon_command;
pub use list::{ListSessionsConfig, run_list_sessions};

use kmux_client::ssh;
use kmux_client::token::read_local_token;

use crate::cli::{OutputFormat, ResolvedConnection};

/// Resolve connection parameters from CLI arguments.
///
/// Handles three modes: local daemon, SSH negotiation, or direct QUIC.
pub async fn resolve_connection(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
    no_ssh: bool,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    accept_invalid_certs: bool,
) -> anyhow::Result<ResolvedConnection> {
    let is_local = server.is_none()
        && host_override.is_none()
        && port_override.is_none()
        && token_override.is_none();

    let parsed = server.map(ssh::parse_server_string);

    // Detect SSH mode: server has a user or matches a hosts.toml alias with a user,
    // and --no-ssh is not given.
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
            Ok(session) => {
                let host = "127.0.0.1".to_string();
                let port = session.local_tcp_port;
                let token = session.token.clone();
                Ok(ResolvedConnection {
                    host,
                    port,
                    tcp_port: None,
                    token,
                    accept_invalid_certs: true,
                    is_local: false,
                    ssh_session: Some(session),
                    ssh_target: Some(target),
                    parsed_server: parsed,
                })
            }
            Err(e) => Err(anyhow::anyhow!("SSH negotiation failed: {e}")),
        }
    } else if is_local {
        let status = kmux_client::daemon::ensure_daemon().await?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: status.port,
            tcp_port: Some(status.tcp_port),
            token: status.token,
            accept_invalid_certs: true,
            is_local: true,
            ssh_session: None,
            ssh_target: None,
            parsed_server: parsed,
        })
    } else {
        // Direct QUIC: positional server (host:port) or explicit --host/--port.
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
            accept_invalid_certs,
            is_local: false,
            ssh_session: None,
            ssh_target: None,
            parsed_server: parsed,
        })
    }
}

pub fn print_sessions(sessions: &[kmux_protocol::messages::SessionEntry], format: &OutputFormat) {
    match format {
        OutputFormat::Table => {
            if sessions.is_empty() {
                println!("No active sessions");
                return;
            }
            println!("{:<16} {:<10} {:<40} {:<6}", "NAME", "ID", "CWD", "PANES");
            for entry in sessions {
                println!(
                    "{:<16} {:<10} {:<40} {:<6}",
                    entry.meta.name,
                    entry.meta.word_id,
                    entry.meta.cwd,
                    entry.panes.len(),
                );
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(sessions).expect("sessions are serializable");
            println!("{json}");
        }
    }
}

pub fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m {s}s")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}
