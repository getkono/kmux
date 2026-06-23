//! `kmux ps` — headless process overview (issue #122).
//!
//! The hierarchical counterpart of `kmux ls`: connects to the daemon, fetches
//! the session list *and* the process snapshot, joins them with the same
//! [`build_overview_rows`](crate::core::build_overview_rows) projection the GUIs
//! use, and prints the Session → Tab → Pane → Process tree. This is the primary
//! headless surface for verifying the feature end-to-end.

use crate::cli::OutputFormat;
use crate::core::build_overview_rows;

use super::{authenticate, render, resolve_connection};

pub struct ProcessOverviewConfig<'a> {
    pub server: Option<&'a str>,
    pub ssh_port: Option<u16>,
    pub format: OutputFormat,
}

pub async fn run_process_overview(cfg: ProcessOverviewConfig<'_>) -> anyhow::Result<()> {
    let conn = resolve_connection(cfg.server, cfg.ssh_port).await?;
    let format = cfg.format;

    use kmux_protocol::messages::{ClientMessage, PaneProcesses, ServerMessage, SessionEntry};
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
    use tokio::net::TcpStream;

    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;
    let (mut read_half, mut write_half) = stream.into_split();

    // Authenticate (token + cryptographic identity challenge–response, issue #146).
    authenticate(&mut read_half, &mut write_half, conn.token).await?;

    // Fetch the session list (for the Session → Tab → Pane hierarchy).
    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::SessionList { request_id: 1 })?,
    )
    .await?;
    let sessions: Vec<SessionEntry> = loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before session list"))?;
        if let ServerMessage::SessionListResult { sessions, .. } = decode_server(&data)? {
            break sessions;
        }
    };

    // Fetch the process snapshot, then join the two with the shared projection.
    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::ProcessOverview { request_id: 2 })?,
    )
    .await?;
    let panes: Vec<PaneProcesses> = loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before process overview"))?;
        if let ServerMessage::ProcessOverviewResult { panes, .. } = decode_server(&data)? {
            break panes;
        }
    };

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
