//! `kmux clients` / `kmux kick` — headless client management (issue #146).
//!
//! Mirrors the raw-TCP approach of [`super::list`]: connect, run the identity
//! handshake, then exchange `ClientList` / `KickClient` with the daemon. For a
//! federated (remote) session the daemon forwards these to the owning peer.

use kmux_protocol::messages::{ClientId, ClientInfo, ClientMessage, ServerMessage};
use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
use tokio::net::TcpStream;

use crate::cli::OutputFormat;

use super::{authenticate, render, resolve_connection};

pub struct ListClientsConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub session: Option<String>,
    pub format: OutputFormat,
}

pub struct KickClientConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub session: String,
    pub client: String,
}

pub async fn run_list_clients(cfg: ListClientsConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;

    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;
    let (mut read_half, mut write_half) = stream.into_split();
    authenticate(&mut read_half, &mut write_half, conn.token).await?;

    // Which sessions to query: the named one, or every session.
    let words: Vec<String> = match cfg.session {
        Some(w) => vec![w],
        None => {
            write_frame(
                &mut write_half,
                &encode_client(&ClientMessage::SessionList { request_id: 1 })?,
            )
            .await?;
            loop {
                let data = read_frame(&mut read_half)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Connection closed before session list"))?;
                if let ServerMessage::SessionListResult { sessions, .. } = decode_server(&data)? {
                    break sessions.into_iter().map(|e| e.meta.word_id).collect();
                }
            }
        }
    };

    // Query each session's connected clients.
    let mut entries: Vec<(String, Vec<ClientInfo>)> = Vec::new();
    for (i, word) in words.into_iter().enumerate() {
        let request_id = 101 + i as u64;
        write_frame(
            &mut write_half,
            &encode_client(&ClientMessage::ClientList {
                request_id,
                word_id: word.clone(),
            })?,
        )
        .await?;
        loop {
            let data = read_frame(&mut read_half)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Connection closed before client list"))?;
            match decode_server(&data)? {
                ServerMessage::ClientListResult {
                    clients, word_id, ..
                } if word_id == word => {
                    entries.push((word.clone(), clients));
                    break;
                }
                ServerMessage::Error { message, .. } => {
                    anyhow::bail!("Failed to list clients for {word}: {message}");
                }
                _ => continue,
            }
        }
    }

    match cfg.format {
        OutputFormat::Json => render::render_json(&entries),
        OutputFormat::Table => {
            let rows = render::client_rows(&entries);
            render::render(&rows, &cfg.format, "No connected clients");
        }
    }
    Ok(())
}

pub async fn run_kick_client(cfg: KickClientConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;

    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;
    let (mut read_half, mut write_half) = stream.into_split();
    authenticate(&mut read_half, &mut write_half, conn.token).await?;

    // Fetch the session's clients so we can resolve the target by label or id.
    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::ClientList {
            request_id: 1,
            word_id: cfg.session.clone(),
        })?,
    )
    .await?;
    let clients = loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before client list"))?;
        match decode_server(&data)? {
            ServerMessage::ClientListResult { clients, .. } => break clients,
            ServerMessage::Error { message, .. } => {
                anyhow::bail!("Failed to list clients: {message}")
            }
            _ => continue,
        }
    };

    let target = resolve_target(&clients, &cfg.client, &cfg.session)?;

    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::KickClient {
            request_id: 2,
            word_id: cfg.session.clone(),
            client_id: target,
        })?,
    )
    .await?;
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before kick result"))?;
        match decode_server(&data)? {
            ServerMessage::ClientKicked { client_id, .. } if client_id == target => {
                println!("Kicked client {} from session {}", target.0, cfg.session);
                return Ok(());
            }
            ServerMessage::Error { message, .. } => anyhow::bail!("Kick failed: {message}"),
            _ => continue,
        }
    }
}

/// Resolve a CLI client argument (a numeric client-id or an exact label) to a
/// [`ClientId`] among `clients`, erroring on no/ambiguous match.
fn resolve_target(clients: &[ClientInfo], arg: &str, session: &str) -> anyhow::Result<ClientId> {
    if let Ok(n) = arg.parse::<u64>() {
        let id = ClientId(n);
        if clients.iter().any(|c| c.client_id == id) {
            return Ok(id);
        }
        anyhow::bail!("No client with id {n} in session {session}");
    }
    let matches: Vec<&ClientInfo> = clients.iter().filter(|c| c.label == arg).collect();
    match matches.as_slice() {
        [c] => Ok(c.client_id),
        [] => anyhow::bail!("No client labelled {arg:?} in session {session}"),
        _ => anyhow::bail!(
            "Ambiguous client {arg:?} in session {session} (multiple matches); use the numeric id"
        ),
    }
}
