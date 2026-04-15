use std::io;

use kmux_client::ssh;
use kmux_client::token::read_local_token;

use crate::cli::{DaemonAction, OutputFormat, ResolvedConnection};

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
            Err(e) => {
                eprintln!("SSH negotiation failed: {e}");
                std::process::exit(1);
            }
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

pub async fn run_daemon_command(action: DaemonAction) -> anyhow::Result<()> {
    match action {
        DaemonAction::Start => {
            // Check if already running.
            if let Some(status) = kmux_client::daemon::query_daemon().await {
                println!(
                    "Daemon already running — PID {}, port {}",
                    status.pid, status.port
                );
                return Ok(());
            }
            let status = kmux_client::daemon::ensure_daemon().await?;
            println!("Daemon started — PID {}, port {}", status.pid, status.port);
        }

        DaemonAction::Stop => {
            kmux_client::daemon::stop_daemon().await.map_err(|e| {
                anyhow::anyhow!("Daemon is not running or could not be stopped: {e}")
            })?;
            println!("Daemon stopped");
        }

        DaemonAction::Status => match kmux_client::daemon::query_daemon().await {
            Some(status) => {
                println!("Status:   running");
                println!("PID:      {}", status.pid);
                println!("Port:     {}", status.port);
                println!("Uptime:   {}", format_uptime(status.uptime_secs));
                println!("Sessions: {}", status.session_count);
            }
            None => {
                println!("Status:   not running");
                std::process::exit(1);
            }
        },

        DaemonAction::Restart => {
            // Stop (ignore "not running").
            let _ = kmux_client::daemon::stop_daemon().await;
            // Poll until the old daemon is confirmed dead (up to 3 seconds).
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if kmux_client::daemon::query_daemon().await.is_none() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for daemon to stop");
                }
            }
            let status = kmux_client::daemon::ensure_daemon().await?;
            println!(
                "Daemon restarted — PID {}, port {}",
                status.pid, status.port
            );
        }

        DaemonAction::Logs { follow } => {
            let log_path = kmux_protocol::dirs::daemon_log_path()?;
            if !log_path.exists() {
                eprintln!(
                    "Log file not found: {}\nHas the daemon been run at least once?",
                    log_path.display()
                );
                std::process::exit(1);
            }

            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&log_path).await?;

            // Print all existing content.
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            io::Write::write_all(&mut io::stdout(), &buf)?;

            if follow {
                // Seek to end and poll for new bytes.
                file.seek(std::io::SeekFrom::End(0)).await?;
                let mut read_buf = vec![0u8; 4096];
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let n = file.read(&mut read_buf).await?;
                    if n > 0 {
                        io::Write::write_all(&mut io::stdout(), &read_buf[..n])?;
                        io::Write::flush(&mut io::stdout())?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_list_sessions(
    server: Option<&str>,
    ssh_port: Option<u16>,
    format: OutputFormat,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    no_ssh: bool,
    accept_invalid_certs: bool,
) -> anyhow::Result<()> {
    let conn = resolve_connection(
        server,
        ssh_port,
        no_ssh,
        host_override,
        port_override,
        token_override,
        accept_invalid_certs,
    )
    .await?;

    // Connect headlessly via TCP, send auth + SessionList, print results.
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, PROTOCOL_VERSION, ServerMessage, version_mismatch_hint,
    };
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
    use tokio::net::TcpStream;

    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;

    let (mut read_half, mut write_half) = stream.into_split();

    // Authenticate.
    let auth_msg = ClientMessage::Auth {
        token: conn.token,
        protocol_version: PROTOCOL_VERSION,
        capabilities: ClientCapabilities::default(),
        connection_id: None,
    };
    let auth_bytes = encode_client(&auth_msg)?;
    write_frame(&mut write_half, &auth_bytes).await?;

    // Wait for AuthResult.
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before auth response"))?;
        let msg = decode_server(&data)?;
        match msg {
            ServerMessage::AuthResult {
                success: true,
                client_id,
                ..
            } => {
                tracing::debug!(?client_id, "Authenticated for list-sessions");
                break;
            }
            ServerMessage::AuthResult {
                success: false,
                reason,
                ..
            } => {
                let reason_str = reason.unwrap_or_else(|| "unknown error".into());
                let hint = version_mismatch_hint(&reason_str);
                if hint.is_empty() {
                    anyhow::bail!("Authentication failed: {reason_str}");
                } else {
                    anyhow::bail!("Authentication failed: {reason_str}\n{hint}");
                }
            }
            _ => continue,
        }
    }

    // Request session list.
    let list_msg = ClientMessage::SessionList { request_id: 1 };
    let list_bytes = encode_client(&list_msg)?;
    write_frame(&mut write_half, &list_bytes).await?;

    // Wait for SessionListResult.
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before session list"))?;
        let msg = decode_server(&data)?;
        match msg {
            ServerMessage::SessionListResult { sessions, .. } => {
                print_sessions(&sessions, &format);
                return Ok(());
            }
            _ => continue,
        }
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
