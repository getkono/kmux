use crate::cli::OutputFormat;

use super::{render, resolve_connection};

pub struct ListSessionsConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub format: OutputFormat,
}

pub async fn run_list_sessions(cfg: ListSessionsConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;
    let format = cfg.format;

    // Connect headlessly via TCP, send auth + SessionList, print results.
    use kmux_protocol::messages::{ClientMessage, ServerMessage};

    let (mut read_half, mut write_half) = super::connect_authenticated(&conn).await?;

    let sessions = super::request_reply(
        &mut read_half,
        &mut write_half,
        &ClientMessage::SessionList { request_id: 1 },
        "session list",
        |msg| match msg {
            ServerMessage::SessionListResult { sessions, .. } => Some(sessions),
            _ => None,
        },
    )
    .await?;

    match format {
        OutputFormat::Table => {
            let rows = render::session_rows(&sessions);
            render::render(&rows, &format, "No active sessions");
        }
        OutputFormat::Json => render::render_json(&sessions),
    }
    Ok(())
}
