use kmux_protocol::control_rpc::{ConnectionInfo, SessionsResponse};
use kmux_protocol::messages::{ClientInfo, SessionEntry};
use kmux_sys::identity;
use tabled::Table;
use tabled::Tabled;
use tabled::settings::Style;

use crate::cli::OutputFormat;
use crate::core::{OverviewRow, OverviewRowKind};

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
            // PEER already carries the machine, so show the undecorated name.
            let name = e.base_name().to_string();
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

// ─── Client row (kmux clients) ────────────────────────────────────────────────

#[derive(Tabled)]
pub struct ClientRow {
    #[tabled(rename = "SESSION")]
    pub session: String,
    #[tabled(rename = "CLIENT")]
    pub client: String,
    #[tabled(rename = "ID")]
    pub id: u64,
    #[tabled(rename = "MACHINE")]
    pub machine: String,
    #[tabled(rename = "FRONTEND")]
    pub frontend: String,
    #[tabled(rename = "BUILD")]
    pub build: String,
    #[tabled(rename = "TRANSPORT")]
    pub transport: String,
    #[tabled(rename = "PANES")]
    pub panes: String,
}

/// Build the `kmux clients` rows. `entries` pairs each session word-id with the
/// connections attached to it (issue #146). The requester's own connection is
/// suffixed with " (you)"; the machine id is abbreviated for display.
pub fn client_rows(entries: &[(String, Vec<ClientInfo>)]) -> Vec<ClientRow> {
    let mut rows = Vec::new();
    for (word_id, clients) in entries {
        for c in clients {
            let client = if c.is_self {
                format!("{} (you)", c.label)
            } else {
                c.label.clone()
            };
            let panes = c
                .attached_panes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            // Build identity (protocol 37): `<sha>[-dirty] (profile)`. Empty for
            // a client too old to report it (older builds can't connect, so this
            // is mostly a defensive fallback).
            let build = match (c.build.as_str(), c.build_profile.as_str()) {
                ("", _) => "<unknown>".to_string(),
                (sha, "") => sha.to_string(),
                (sha, profile) => format!("{sha} ({profile})"),
            };
            rows.push(ClientRow {
                session: word_id.clone(),
                client,
                id: c.client_id.0,
                machine: identity::short(&c.machine_id).to_string(),
                frontend: c.frontend.to_string(),
                build,
                transport: c.transport.clone(),
                panes,
            });
        }
    }
    rows
}

// ─── Process overview row (kmux ps) ───────────────────────────────────────────

#[derive(Tabled)]
pub struct ProcessRow {
    #[tabled(rename = "NAME")]
    pub name: String,
    #[tabled(rename = "CPU%")]
    pub cpu: String,
    #[tabled(rename = "MEM")]
    pub mem: String,
    #[tabled(rename = "PID")]
    pub pid: String,
}

/// Build the `kmux ps` table rows from the flat overview projection (issue #122),
/// rendering the hierarchy by indenting NAME on each row's depth. Session rows
/// also surface a peer/cwd hint so federated machines are distinguishable.
pub fn process_overview_rows(rows: &[OverviewRow]) -> Vec<ProcessRow> {
    rows.iter()
        .map(|r| {
            let indent = "  ".repeat(r.depth as usize);
            let mut name = format!("{indent}{}", r.label);
            if r.kind == OverviewRowKind::Session {
                match &r.peer {
                    Some(peer) => name.push_str(&format!("  ({peer})")),
                    None if !r.detail.is_empty() => name.push_str(&format!("  ({})", r.detail)),
                    None => {}
                }
            }
            ProcessRow {
                name,
                cpu: format!("{:.1}", r.cpu_percent),
                mem: crate::humanize::bytes(r.mem_bytes),
                pid: r.pid.map_or_else(|| "-".into(), |p| p.to_string()),
            }
        })
        .collect()
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
        bytes_in: crate::humanize::bytes(conn.bytes_in),
        bytes_out: crate::humanize::bytes(conn.bytes_out),
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

// ─── Stop summary row (kmux daemon stop) ──────────────────────────────────────

#[derive(Tabled)]
pub struct StopSummaryRow {
    #[tabled(rename = "SESSION")]
    pub session: String,
    #[tabled(rename = "PANES")]
    pub panes: usize,
    #[tabled(rename = "CLIENTS")]
    pub clients: String,
    #[tabled(rename = "RUNNING")]
    pub running: String,
}

/// Build the pre-shutdown summary shown by `kmux daemon stop` so the user sees
/// exactly what they are about to terminate.
///
/// Sessions, pane counts, and attached clients come from the daemon's
/// control-socket snapshot (`resp`). `processes_by_session` maps a session
/// word-id to the distinct process names running across its panes; it is a
/// best-effort data-plane enrichment, so it is empty when that query was skipped
/// or failed — the RUNNING column then degrades to `-` and the summary still
/// shows sessions and clients.
pub fn stop_summary_rows(
    resp: &SessionsResponse,
    processes_by_session: &std::collections::HashMap<String, Vec<String>>,
) -> Vec<StopSummaryRow> {
    resp.sessions
        .iter()
        .map(|sc| {
            let clients = if sc.connections.is_empty() {
                "(none)".to_string()
            } else {
                let labels: Vec<String> = sc.connections.iter().map(|c| c.label.clone()).collect();
                truncate_join(&labels, 2)
            };
            let running = match processes_by_session.get(&sc.meta.word_id) {
                Some(names) if !names.is_empty() => truncate_join(names, 4),
                _ => "-".to_string(),
            };
            StopSummaryRow {
                session: sc.meta.name.clone(),
                panes: sc.panes_count,
                clients,
                running,
            }
        })
        .collect()
}

/// Join up to `max` items with ", ", appending " +N" when more were elided.
fn truncate_join(items: &[String], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        format!("{}, +{}", items[..max].join(", "), items.len() - max)
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

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::control_rpc::{SessionConnections, SessionsResponse};
    use kmux_protocol::messages::SessionMeta;

    #[test]
    fn process_overview_rows_indent_and_format() {
        let rows = vec![
            OverviewRow {
                depth: 0,
                kind: OverviewRowKind::Session,
                label: "dev".into(),
                detail: "/proj".into(),
                cpu_percent: 12.0,
                mem_bytes: 2 * 1024 * 1024,
                pid: None,
                peer: None,
            },
            OverviewRow {
                depth: 3,
                kind: OverviewRowKind::Process,
                label: "cargo".into(),
                detail: "cargo build".into(),
                cpu_percent: 9.5,
                mem_bytes: 1024,
                pid: Some(101),
                peer: None,
            },
        ];
        let table = process_overview_rows(&rows);
        // Session row: no indent, cwd hint appended, no pid.
        assert_eq!(table[0].name, "dev  (/proj)");
        assert_eq!(table[0].cpu, "12.0");
        assert_eq!(table[0].mem, "2.0 MiB");
        assert_eq!(table[0].pid, "-");
        // Process row: indented by depth, pid shown.
        assert_eq!(table[1].name, "      cargo");
        assert_eq!(table[1].cpu, "9.5");
        assert_eq!(table[1].pid, "101");
    }

    #[test]
    fn process_overview_rows_session_shows_peer() {
        let rows = vec![OverviewRow {
            depth: 0,
            kind: OverviewRowKind::Session,
            label: "api".into(),
            detail: "/srv".into(),
            cpu_percent: 0.0,
            mem_bytes: 0,
            pid: None,
            peer: Some("bob@host".into()),
        }];
        let table = process_overview_rows(&rows);
        assert_eq!(table[0].name, "api  (bob@host)");
    }

    fn make_conn(conn_id: u64, transport: &str) -> ConnectionInfo {
        ConnectionInfo {
            connection_id: conn_id,
            client_id: 1,
            transport: transport.to_string(),
            label: "alice@host".to_string(),
            machine_id: "abc123".to_string(),
            hostname: "host".to_string(),
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
    fn stop_summary_joins_clients_and_processes() {
        use std::collections::HashMap;

        let resp = SessionsResponse {
            sessions: vec![
                SessionConnections {
                    meta: make_meta("work", "eagle"),
                    panes_count: 3,
                    connections: vec![make_conn(1, "quic"), make_conn(2, "uds")],
                },
                SessionConnections {
                    meta: make_meta("idle", "hippo"),
                    panes_count: 1,
                    connections: vec![],
                },
            ],
            unattached: vec![],
        };
        let mut procs: HashMap<String, Vec<String>> = HashMap::new();
        procs.insert(
            "eagle".to_string(),
            vec!["nvim".into(), "cargo".into(), "ssh".into()],
        );
        // 'hippo' intentionally absent → its RUNNING degrades to "-".

        let rows = stop_summary_rows(&resp, &procs);
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].session, "work");
        assert_eq!(rows[0].panes, 3);
        // Two identical labels in this fixture; both shown (cap is 2).
        assert_eq!(rows[0].clients, "alice@host, alice@host");
        assert_eq!(rows[0].running, "nvim, cargo, ssh");

        // No clients and no process data → explicit placeholders, never blank.
        assert_eq!(rows[1].clients, "(none)");
        assert_eq!(rows[1].running, "-");
    }

    #[test]
    fn truncate_join_elides_overflow() {
        let three = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(truncate_join(&three, 2), "a, b, +1");
        assert_eq!(truncate_join(&three, 3), "a, b, c");
        assert_eq!(truncate_join(&three, 5), "a, b, c");
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
                progress_state: Default::default(),
                progress: None,
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
