//! `kmux ps` — headless process overview (issue #122).
//!
//! The hierarchical counterpart of `kmux ls`: connects to the daemon, fetches
//! the session list *and* the process snapshot, joins them with the same
//! [`build_overview_rows`](crate::core::build_overview_rows) projection the GUIs
//! use, and prints the Session → Tab → Pane → Process tree. This is the primary
//! headless surface for verifying the feature end-to-end.

use crate::cli::OutputFormat;
use crate::core::build_overview_rows;

use super::{render, resolve_connection};

pub struct ProcessOverviewConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub format: OutputFormat,
}

pub async fn run_process_overview(cfg: ProcessOverviewConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;
    let format = cfg.format;

    use kmux_protocol::messages::{ClientMessage, PaneProcesses, ServerMessage, SessionEntry};

    let (mut read_half, mut write_half) = super::connect_authenticated(&conn).await?;

    // Fetch the session list (for the Session → Tab → Pane hierarchy).
    let sessions: Vec<SessionEntry> = super::request_reply(
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

    // Fetch the process snapshot, then join the two with the shared projection.
    let panes: Vec<PaneProcesses> = super::request_reply(
        &mut read_half,
        &mut write_half,
        &ClientMessage::ProcessOverview { request_id: 2 },
        "process overview",
        |msg| match msg {
            ServerMessage::ProcessOverviewResult { panes, .. } => Some(panes),
            _ => None,
        },
    )
    .await?;

    match format {
        OutputFormat::Table => {
            let rows = build_overview_rows(&sessions, &panes);
            let table_rows = render::process_overview_rows(&rows);
            render::render(&table_rows, &format, "No active sessions");
        }
        // JSON emits the raw per-pane process trees (pane_id encodes the
        // session), the complete structured snapshot.
        OutputFormat::Json => render::render_json(&panes),
    }
    Ok(())
}
