//! `kmux status` — one health view across every kmux process.
//!
//! Aggregates the local daemon (`kmuxd`), the GUI client singleton
//! (`kmux-swift` / `kmux-gtk`), this CLI, and any isolated per-pane VT workers
//! into a single report, classifying compatibility through the shared
//! [`kmux_protocol::compat`] SSoT. `kmux daemon status` and `kmux client status`
//! remain the scoped, detailed views; this is the at-a-glance overview.
//!
//! Exit code: non-zero when the daemon is not running or a *blocking*
//! (protocol/profile) skew is present — matching `kmux daemon status`. A
//! not-running GUI and build-fingerprint skew are informational.

use kmux_protocol::compat::{self, Match3};
use kmux_protocol::control_rpc::WorkerInfo;
use kmux_protocol::dirs::BuildProfile;
use kmux_protocol::messages::PROTOCOL_RANGE;

use crate::cli::OutputFormat;

use super::render;

/// GUI client process name for this platform, or `None` where the GUI is not yet
/// supported (Windows). Debug and release builds share the name; the per-profile
/// daemon-socket split is what keeps a debug and release client from colliding.
#[cfg(target_os = "macos")]
pub(crate) const GUI_PROCESS: Option<&str> = Some("kmux-swift");
#[cfg(target_os = "linux")]
pub(crate) const GUI_PROCESS: Option<&str> = Some("kmux-gtk");
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) const GUI_PROCESS: Option<&str> = None;

/// PIDs of running GUI client processes (via `pgrep -x`). Empty on unsupported
/// platforms or when none run.
pub(crate) fn gui_pids() -> Vec<u32> {
    let Some(name) = GUI_PROCESS else {
        return Vec::new();
    };
    match std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .filter_map(|s| s.parse::<u32>().ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// This machine's cryptographic identity (the `machine_id` the daemon records),
/// so we can pick out *our* GUI connection from the registry.
pub(crate) fn local_machine_id() -> Option<String> {
    kmux_protocol::identity::Identity::load_or_create()
        .ok()
        .map(|id| id.fingerprint())
}

/// `<build> (<profile>)`, or `<unknown> (<profile>)` for an empty build.
pub(crate) fn build_display(build: &str, profile: &str) -> String {
    let b = if build.is_empty() { "<unknown>" } else { build };
    if profile.is_empty() {
        b.to_string()
    } else {
        format!("{b} ({profile})")
    }
}

// ── JSON report ─────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct StatusReport {
    /// True when the daemon is running and there is no blocking skew — mirrors
    /// the process exit code.
    ok: bool,
    daemon: DaemonSection,
    client: ClientSection,
    workers: WorkersSection,
    cli: CliSection,
    /// Human-readable skew warnings (also printed in table mode).
    warnings: Vec<String>,
}

#[derive(serde::Serialize)]
struct DaemonSection {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
}

#[derive(serde::Serialize)]
struct ClientSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    frontend: Option<String>,
    running: bool,
    pids: Vec<u32>,
    attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
}

#[derive(serde::Serialize)]
struct WorkersSection {
    /// `"process"` / `"in-process"`, `"unavailable"` (daemon predates worker
    /// reporting), or `"daemon-down"`.
    isolation_mode: String,
    workers: Vec<WorkerInfo>,
}

#[derive(serde::Serialize)]
struct CliSection {
    build: String,
    profile: String,
    protocol: String,
}

// ── Command ─────────────────────────────────────────────────────────────────

pub async fn run_status(format: OutputFormat) -> anyhow::Result<()> {
    let cli_build = kmux_protocol::buildinfo::fingerprint();
    let cli_profile = kmux_protocol::buildinfo::build_profile();

    let daemon = kmux_client::daemon::query_daemon().await;

    // The GUI client and worker reporting both go through the daemon registry,
    // so only query them when the daemon answered (never dial a dead socket).
    let pids = gui_pids();
    let local_mid = local_machine_id();
    let (gui_conn, workers) = if daemon.is_some() {
        let gui = kmux_client::daemon::query_connections()
            .await
            .ok()
            .and_then(|resp| {
                resp.connections.into_iter().find(|c| {
                    c.frontend != "cli" && local_mid.as_deref().is_none_or(|m| m == c.machine_id)
                })
            });
        let workers = kmux_client::daemon::query_workers().await.ok();
        (gui, workers)
    } else {
        (None, None)
    };

    // Blocking skew gates the exit code, exactly like `kmux daemon status`.
    let blocking = daemon
        .as_ref()
        .and_then(|d| compat::attach_block(d.protocol_range, d.build_profile));
    let ok = daemon.is_some() && blocking.is_none();

    let warnings = collect_warnings(daemon.as_ref(), gui_conn.as_ref(), &cli_build);

    let workers_section = match (&daemon, &workers) {
        (None, _) => WorkersSection {
            isolation_mode: "daemon-down".to_string(),
            workers: Vec::new(),
        },
        (Some(_), None) => WorkersSection {
            isolation_mode: "unavailable".to_string(),
            workers: Vec::new(),
        },
        (Some(_), Some(w)) => WorkersSection {
            isolation_mode: w.isolation_mode.clone(),
            workers: w.workers.clone(),
        },
    };

    let report = StatusReport {
        ok,
        daemon: DaemonSection {
            running: daemon.is_some(),
            pid: daemon.as_ref().map(|d| d.pid),
            port: daemon.as_ref().map(|d| d.port),
            uptime_secs: daemon.as_ref().map(|d| d.uptime_secs),
            session_count: daemon.as_ref().map(|d| d.session_count),
            protocol: daemon.as_ref().map(|d| {
                d.protocol_range.map_or_else(
                    || format!("legacy-{}", d.protocol_version),
                    |range| range.to_string(),
                )
            }),
            version: daemon.as_ref().map(|d| d.kmuxd_version.clone()),
            build: daemon
                .as_ref()
                .filter(|d| !d.kmuxd_build.is_empty())
                .map(|d| d.kmuxd_build.clone()),
            profile: daemon
                .as_ref()
                .and_then(|d| d.build_profile)
                .map(|p| p.as_str().to_string()),
        },
        client: ClientSection {
            frontend: GUI_PROCESS.map(str::to_string),
            running: !pids.is_empty(),
            pids: pids.clone(),
            attached: gui_conn.is_some(),
            build: gui_conn
                .as_ref()
                .filter(|c| !c.build.is_empty())
                .map(|c| c.build.clone()),
            profile: gui_conn
                .as_ref()
                .filter(|c| !c.build_profile.is_empty())
                .map(|c| c.build_profile.clone()),
            label: gui_conn.as_ref().map(|c| c.label.clone()),
            transport: gui_conn.as_ref().map(|c| c.transport.clone()),
        },
        workers: workers_section,
        cli: CliSection {
            build: cli_build.clone(),
            profile: cli_profile.to_string(),
            protocol: PROTOCOL_RANGE.to_string(),
        },
        warnings,
    };

    match format {
        OutputFormat::Json => render::render_json(&report),
        OutputFormat::Table => print_table(&report),
    }

    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Skew warnings, classified through the shared `compat` SSoT. The same
/// dimensions `kmux client status` reports, phrased for the overview.
fn collect_warnings(
    daemon: Option<&kmux_client::daemon::DaemonStatus>,
    gui_conn: Option<&kmux_protocol::control_rpc::ConnectionSummary>,
    cli_build: &str,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(d) = daemon else {
        return warnings;
    };
    if compat::protocol_match(d.protocol_range) != Match3::Same {
        warnings.push(format!(
            "protocol skew: CLI {PROTOCOL_RANGE} vs daemon {} — they cannot connect. \
             Run `kmux daemon restart` or reinstall kmux.",
            d.protocol_range.map_or_else(
                || format!("legacy-{}", d.protocol_version),
                |range| range.to_string()
            )
        ));
    }
    if compat::profile_match(d.build_profile) != Match3::Same {
        warnings.push(format!(
            "profile skew: CLI {} vs daemon {}.",
            BuildProfile::CURRENT,
            d.build_profile.map(|p| p.as_str()).unwrap_or("<unknown>"),
        ));
    }
    if compat::build_match(&d.kmuxd_build, cli_build) == Match3::Differ {
        warnings.push(format!(
            "build skew: CLI {cli_build} differs from daemon {}. Reinstall kmux so they match.",
            d.kmuxd_build
        ));
    }
    if let Some(c) = gui_conn
        && !d.kmuxd_build.is_empty()
        && compat::build_match(&c.build, &d.kmuxd_build) == Match3::Differ
    {
        warnings.push(format!(
            "GUI build {} differs from daemon {}. Restart it: `kmux client restart`.",
            c.build, d.kmuxd_build
        ));
    }
    warnings
}

fn print_table(r: &StatusReport) {
    // ── Daemon ──────────────────────────────────────────────────────────────
    println!("Daemon:");
    if r.daemon.running {
        if let Some(pid) = r.daemon.pid {
            println!("  Status:   running (PID {pid})");
        }
        if let Some(port) = r.daemon.port {
            println!("  Port:     {port}");
        }
        if let Some(u) = r.daemon.uptime_secs {
            println!("  Uptime:   {}", render::format_uptime(u));
        }
        if let Some(n) = r.daemon.session_count {
            println!("  Sessions: {n}");
        }
        if let Some(p) = &r.daemon.protocol {
            println!("  Protocol: {p}");
        }
        let build = r.daemon.build.clone().unwrap_or_else(|| "<unknown>".into());
        let profile = r
            .daemon
            .profile
            .clone()
            .unwrap_or_else(|| "<unknown>".into());
        println!("  Build:    {}", build_display(&build, &profile));
        if let Some(v) = &r.daemon.version {
            println!("  Version:  {v}");
        }
    } else {
        println!("  Status:   not running");
    }

    // ── Client (the GUI singleton) ──────────────────────────────────────────
    println!("Client:");
    println!(
        "  Frontend: {}",
        r.client
            .frontend
            .as_deref()
            .unwrap_or("<unsupported on this platform>")
    );
    if r.client.running {
        let list = r
            .client
            .pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Process:  running (PID {list})");
    } else {
        println!("  Process:  not running");
    }
    match (r.daemon.running, &r.client.build) {
        (true, Some(build)) => {
            let profile = r.client.profile.clone().unwrap_or_default();
            println!("  Build:    {}", build_display(build, &profile));
            if let (Some(label), Some(transport)) = (&r.client.label, &r.client.transport) {
                println!("  Attached: {label} ({transport})");
            }
        }
        (true, None) if r.client.running => {
            println!("  Build:    <unknown — GUI running but not attached to the local daemon>");
        }
        (true, None) => println!("  Build:    <none — no GUI client attached>"),
        (false, _) => println!("  Build:    <unknown — local daemon not running>"),
    }

    // ── Workers (isolated per-pane VT subprocesses) ─────────────────────────
    println!("Workers:");
    match r.workers.isolation_mode.as_str() {
        "daemon-down" => println!("  Isolation: <unknown — local daemon not running>"),
        "unavailable" => {
            println!("  Isolation: unavailable (daemon predates worker reporting)");
        }
        "process" => {
            if r.workers.workers.is_empty() {
                println!("  Isolation: process (no panes running yet)");
            } else {
                println!("  Isolation: process");
                for w in &r.workers.workers {
                    let budget = if w.within_restart_budget {
                        String::new()
                    } else {
                        " (restart budget exhausted)".to_string()
                    };
                    println!(
                        "    {pane}  pid={pid}  {status}  restarts={n}{budget}",
                        pane = w.pane_id,
                        pid = w.worker_pid,
                        status = w.status,
                        n = w.restart_count,
                    );
                }
            }
        }
        // "in-process" and any future mode.
        other => println!("  Isolation: {other} (no isolated workers)"),
    }

    // ── This CLI ────────────────────────────────────────────────────────────
    println!("CLI:");
    println!(
        "  Build:    {}",
        build_display(&r.cli.build, &r.cli.profile)
    );
    println!("  Protocol: {}", r.cli.protocol);

    // ── Verdict ─────────────────────────────────────────────────────────────
    if !r.daemon.running {
        println!("✗  daemon not running — start it with `kmux daemon start`.");
    } else if r.warnings.is_empty() {
        println!("✓  daemon, client, and CLI are compatible.");
    } else {
        for w in &r.warnings {
            println!("⚠  {w}");
        }
    }
}
