use kmux_protocol::control_rpc::{ConnectionInfo, SessionsResponse};
use kmux_protocol::messages::SessionEntry;
use tabled::Table;
use tabled::Tabled;
use tabled::settings::Style;

use crate::cli::OutputFormat;

// ─── Session row (kmux ls) ────────────────────────────────────────────────────

#[derive(Tabled)]
pub struct SessionRow {
    #[tabled(rename = "PEER")]
    pub peer: String,
    #[tabled(rename = "NAME")]
    pub name: String,
    #[tabled(rename = "ID")]
    pub id: String,
    #[tabled(rename = "CWD")]
    pub cwd: String,
    #[tabled(rename = "PANES")]
    pub panes: usize,
}

/// Build the `kmux ls` rows, grouped by peer (issue #121): local sessions first,
/// then each federated remote (alphabetically). The federated `"name @ peer"`
/// decoration is stripped from NAME since PEER already carries it.
pub fn session_rows(sessions: &[SessionEntry]) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = sessions
        .iter()
        .map(|e| {
            let name = match &e.peer {
                Some(p) => e
                    .meta
                    .name
                    .strip_suffix(&format!(" @ {p}"))
                    .unwrap_or(&e.meta.name)
                    .to_string(),
                None => e.meta.name.clone(),
            };
            SessionRow {
                peer: e.peer.clone().unwrap_or_else(|| "local".to_string()),
                name,
                id: e.meta.word_id.clone(),
                cwd: e.meta.cwd.clone(),
                panes: e.panes.len(),
            }
        })
        .collect();
    // local (peer.is_none()) first, then remotes alphabetically by peer.
    rows.sort_by(|a, b| (a.peer != "local", &a.peer).cmp(&(b.peer != "local", &b.peer)));
    rows
}

// ─── Daemon sessions row (kmux daemon sessions) ───────────────────────────────

#[derive(Tabled)]
pub struct DaemonSessionRow {
    #[tabled(rename = "SESSION")]
    pub session: String,
    #[tabled(rename = "ID")]
    pub id: String,
    #[tabled(rename = "CONN")]
    pub conn: String,
    #[tabled(rename = "TRANSPORT")]
    pub transport: String,
    #[tabled(rename = "UPTIME")]
    pub uptime: String,
    #[tabled(rename = "LAST PING")]
    pub last_ping: String,
    #[tabled(rename = "RTT")]
    pub rtt: String,
    #[tabled(rename = "IN")]
    pub bytes_in: String,
    #[tabled(rename = "OUT")]
    pub bytes_out: String,
}

/// Build rows for `kmux daemon sessions`.
///
/// When `all` is false, sessions with no active connections are omitted.
/// Unattached connections always appear as the last row(s) when `all` is true.
pub fn daemon_session_rows(resp: &SessionsResponse, all: bool) -> Vec<DaemonSessionRow> {
    let mut rows = Vec::new();

    for sc in &resp.sessions {
        if sc.connections.is_empty() {
            if all {
                rows.push(empty_session_row(&sc.meta.name, &sc.meta.word_id));
            }
        } else {
            for conn in &sc.connections {
                rows.push(connection_row(&sc.meta.name, &sc.meta.word_id, conn));
            }
        }
    }

    if all {
        for conn in &resp.unattached {
            rows.push(connection_row("(unattached)", "-", conn));
        }
    }

    rows
}

fn connection_row(session: &str, id: &str, conn: &ConnectionInfo) -> DaemonSessionRow {
    DaemonSessionRow {
        session: session.to_string(),
        id: id.to_string(),
        conn: format!("#{}", conn.connection_id),
        transport: conn.transport.clone(),
        uptime: format_uptime(conn.uptime_secs),
        last_ping: format_ago_ms(conn.last_pong_ago_ms),
        rtt: format_rtt(conn.last_rtt_ms),
        bytes_in: format_bytes(conn.bytes_in),
        bytes_out: format_bytes(conn.bytes_out),
    }
}

fn empty_session_row(session: &str, id: &str) -> DaemonSessionRow {
    DaemonSessionRow {
        session: session.to_string(),
        id: id.to_string(),
        conn: "-".to_string(),
        transport: "-".to_string(),
        uptime: "-".to_string(),
        last_ping: "-".to_string(),
        rtt: "-".to_string(),
        bytes_in: "-".to_string(),
        bytes_out: "-".to_string(),
    }
}

// ─── Generic table / JSON renderer ───────────────────────────────────────────

/// Print a table of `T` rows, or emit a "no items" message if the row slice is
/// empty.  Respects the caller's `OutputFormat` (see `render_json` for JSON).
pub fn render<T: Tabled>(rows: &[T], format: &OutputFormat, empty_message: &str) {
    match format {
        OutputFormat::Table => {
            if rows.is_empty() {
                println!("{empty_message}");
            } else {
                println!("{}", Table::new(rows).with(Style::blank()));
            }
        }
        OutputFormat::Json => {
            // JSON callers should use render_json; this path is unreachable in
            // practice but included for completeness.
            let _ = (rows, empty_message);
        }
    }
}

/// Serialize `value` as pretty JSON and print it.
pub fn render_json<T: serde::Serialize>(value: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable")
    );
}

// ─── Formatting helpers ───────────────────────────────────────────────────────

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

fn format_ago_ms(ago_ms: Option<u64>) -> String {
    match ago_ms {
        None => "-".to_string(),
        Some(ms) => {
            let secs = ms / 1000;
            if secs == 0 {
                format!("{ms}ms")
            } else {
                format!("{secs}s")
            }
        }
    }
}

fn format_rtt(rtt_ms: Option<u64>) -> String {
    match rtt_ms {
        None => "-".to_string(),
        Some(ms) => format!("{ms}ms"),
    }
}

fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::control_rpc::{SessionConnections, SessionsResponse};
    use kmux_protocol::messages::SessionMeta;

    fn make_conn(conn_id: u64, transport: &str) -> ConnectionInfo {
        ConnectionInfo {
            connection_id: conn_id,
            client_id: 1,
            transport: transport.to_string(),
            bytes_in: 1024 * 1024,
            bytes_out: 45 * 1024 * 1024,
            msgs_in: 10,
            msgs_out: 20,
            uptime_secs: 723,
            last_activity_ago_ms: Some(500),
            last_pong_ago_ms: Some(2000),
            last_rtt_ms: Some(2),
        }
    }

    fn make_meta(name: &str, word_id: &str) -> SessionMeta {
        SessionMeta {
            index: 0,
            word_id: word_id.to_string(),
            name: name.to_string(),
            cwd: "/home/user".to_string(),
        }
    }

    #[test]
    fn format_bytes_scales() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn format_ago_ms_variants() {
        assert_eq!(format_ago_ms(None), "-");
        assert_eq!(format_ago_ms(Some(200)), "200ms");
        assert_eq!(format_ago_ms(Some(2000)), "2s");
    }

    #[test]
    fn format_rtt_variants() {
        assert_eq!(format_rtt(None), "-");
        assert_eq!(format_rtt(Some(3)), "3ms");
    }

    #[test]
    fn daemon_session_rows_default_hides_empty_sessions() {
        let resp = SessionsResponse {
            sessions: vec![
                SessionConnections {
                    meta: make_meta("work", "eagle"),
                    panes_count: 1,
                    connections: vec![make_conn(5, "QUIC")],
                },
                SessionConnections {
                    meta: make_meta("idle", "hippo"),
                    panes_count: 1,
                    connections: vec![],
                },
            ],
            unattached: vec![],
        };
        let rows = daemon_session_rows(&resp, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session, "work");
        assert_eq!(rows[0].transport, "QUIC");
        assert_eq!(rows[0].conn, "#5");
    }

    #[test]
    fn daemon_session_rows_all_shows_empty_and_unattached() {
        let resp = SessionsResponse {
            sessions: vec![SessionConnections {
                meta: make_meta("idle", "hippo"),
                panes_count: 1,
                connections: vec![],
            }],
            unattached: vec![make_conn(12, "TCP+TLS")],
        };
        let rows = daemon_session_rows(&resp, true);
        // One empty session row + one unattached row
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session, "idle");
        assert_eq!(rows[0].conn, "-");
        assert_eq!(rows[1].session, "(unattached)");
        assert_eq!(rows[1].transport, "TCP+TLS");
    }

    #[test]
    fn session_rows_maps_fields() {
        use kmux_protocol::messages::{PaneInfo, SessionStatus, TermSize};
        let sessions = vec![SessionEntry {
            meta: make_meta("dev", "eagle"),
            panes: vec![PaneInfo {
                pane_id: "eagle/0".to_string(),
                pane_index: 0,
                program: "zsh".to_string(),
                size: TermSize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                attached_clients: vec![],
                status: SessionStatus::Running,
                title: String::new(),
            }],
            tabs: vec![kmux_protocol::messages::TabInfo {
                tab_index: 0,
                name: "1".to_string(),
                layout: kmux_protocol::messages::LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        }];
        let rows = session_rows(&sessions);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].peer, "local");
        assert_eq!(rows[0].name, "dev");
        assert_eq!(rows[0].id, "eagle");
        assert_eq!(rows[0].panes, 1);
    }

    /// `kmux ls` groups by peer: local sessions first, then federated remotes in
    /// alphabetical order, with the `" @ peer"` decoration stripped from NAME
    /// (issue #121).
    #[test]
    fn session_rows_group_local_first_then_peers() {
        let entry = |name: &str, id: &str, peer: Option<&str>| SessionEntry {
            meta: make_meta(name, id),
            panes: vec![],
            tabs: vec![],
            active_tab: 0,
            peer: peer.map(String::from),
        };
        // Deliberately out of order: a remote, then a local, then another remote.
        let sessions = vec![
            entry("api @ bob@host", "eagle", Some("bob@host")),
            entry("dev", "hippo", None),
            entry("web @ alice@box", "otter", Some("alice@box")),
        ];
        let rows = session_rows(&sessions);
        // local first, then alice@box, then bob@host.
        assert_eq!(rows[0].peer, "local");
        assert_eq!(rows[0].name, "dev");
        assert_eq!(rows[1].peer, "alice@box");
        assert_eq!(rows[1].name, "web", "the ' @ peer' suffix is stripped");
        assert_eq!(rows[2].peer, "bob@host");
        assert_eq!(rows[2].name, "api");
    }
}
