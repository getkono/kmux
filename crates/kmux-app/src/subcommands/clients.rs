//! `kmux clients` / `kmux kick` — headless client management (issue #146).
//!
//! Mirrors the raw-TCP approach of [`super::list`]: connect, run the identity
//! handshake, then exchange `ClientList` / `KickClient` with the daemon. For a
//! federated (remote) session the daemon forwards these to the owning peer.

use kmux_protocol::messages::{ClientId, ClientInfo, ClientMessage, ServerMessage};

use crate::cli::OutputFormat;

use super::{render, resolve_connection};

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

    let (mut read_half, mut write_half) = super::connect_authenticated(&conn).await?;

    // Which sessions to query: the named one, or every session.
    let words: Vec<String> = match cfg.session {
        Some(w) => vec![w],
        None => {
            super::request_reply(
                &mut read_half,
                &mut write_half,
                &ClientMessage::SessionList { request_id: 1 },
                "session list",
                |msg| match msg {
                    ServerMessage::SessionListResult { sessions, .. } => {
                        Some(sessions.into_iter().map(|e| e.meta.word_id).collect())
                    }
                    _ => None,
                },
            )
            .await?
        }
    };

    // Query each session's connected clients.
    let mut entries: Vec<(String, Vec<ClientInfo>)> = Vec::new();
    for (i, word) in words.into_iter().enumerate() {
        let request_id = 101 + i as u64;
        let clients = super::request_reply(
            &mut read_half,
            &mut write_half,
            &ClientMessage::ClientList {
                request_id,
                word_id: word.clone(),
            },
            &format!("client list for {word}"),
            |msg| match msg {
                // The word guard matters: several sessions are queried on one
                // connection, so a late reply for an earlier one must not be
                // mistaken for this one's.
                ServerMessage::ClientListResult {
                    clients, word_id, ..
                } if word_id == word => Some(clients),
                _ => None,
            },
        )
        .await?;
        entries.push((word.clone(), clients));
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

    let (mut read_half, mut write_half) = super::connect_authenticated(&conn).await?;

    // Fetch the session's clients so we can resolve the target by label or id.
    let clients = super::request_reply(
        &mut read_half,
        &mut write_half,
        &ClientMessage::ClientList {
            request_id: 1,
            word_id: cfg.session.clone(),
        },
        "client list",
        |msg| match msg {
            ServerMessage::ClientListResult { clients, .. } => Some(clients),
            _ => None,
        },
    )
    .await?;

    let target = resolve_target(&clients, &cfg.client, &cfg.session)?;

    super::request_reply(
        &mut read_half,
        &mut write_half,
        &ClientMessage::KickClient {
            request_id: 2,
            word_id: cfg.session.clone(),
            client_id: target,
        },
        "kick",
        |msg| match msg {
            ServerMessage::ClientKicked { client_id, .. } if client_id == target => Some(()),
            _ => None,
        },
    )
    .await?;
    println!("Kicked client {} from session {}", target.0, cfg.session);
    Ok(())
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
