use crate::cli::OutputFormat;

use super::{print_sessions, resolve_connection};

pub struct ListSessionsConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub format: OutputFormat,
    pub host_override: Option<&'a str>,
    pub port_override: Option<u16>,
    pub token_override: Option<&'a str>,
    pub no_ssh: bool,
    pub accept_invalid_certs: bool,
}

pub async fn run_list_sessions(cfg: ListSessionsConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(
        cfg.server,
        cfg.ssh_port,
        cfg.no_ssh,
        cfg.host_override,
        cfg.port_override,
        cfg.token_override,
        cfg.accept_invalid_certs,
    )
    .await?;
    let format = cfg.format;

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
