//! `kmux client` — manage *this machine's* singleton GUI client process, the
//! mirror of `kmux daemon` for the client side (issue: client↔daemon build skew).
//!
//! `status` compares the running GUI client, the local daemon, and this CLI by
//! build, warning when they diverge (the silent skew where an installed CLI is
//! older than the daemon it talks to). `logs` tails the client log; `stop` /
//! `restart` drive the singleton process. The GUI clients are singletons (GTK
//! D-Bus app-id in release; Swift `CFBundleIdentifier` + `kmux://` routing), so
//! there is exactly one process to manage per profile.

use std::time::Duration;

use crate::cli::ClientAction;
// GUI-introspection helpers shared with `kmux status` (their SSoT home).
use super::status::{GUI_PROCESS, build_display, gui_pids, local_machine_id};

pub async fn run_client_command(action: ClientAction) -> anyhow::Result<()> {
    match action {
        ClientAction::Status => client_status().await,
        ClientAction::Logs { follow, lines } => client_logs(follow, lines).await,
        ClientAction::Stop => {
            client_stop();
            Ok(())
        }
        ClientAction::Restart => client_restart(),
    }
}

async fn client_status() -> anyhow::Result<()> {
    use kmux_protocol::dirs::BuildProfile;
    use kmux_protocol::messages::PROTOCOL_RANGE;

    let cli_build = kmux_protocol::buildinfo::fingerprint();
    let cli_profile = kmux_protocol::buildinfo::build_profile();

    let daemon = kmux_client::daemon::query_daemon().await;
    let pids = gui_pids();

    // The GUI client's build is learned from the local daemon's connection
    // registry (it has no control socket of its own). Match our machine's
    // non-CLI connection.
    let local_mid = local_machine_id();
    let gui_conn = if daemon.is_some() {
        kmux_client::daemon::query_connections()
            .await
            .ok()
            .and_then(|resp| {
                resp.connections.into_iter().find(|c| {
                    c.frontend != "cli" && local_mid.as_deref().is_none_or(|m| m == c.machine_id)
                })
            })
    } else {
        None
    };

    // ── Client (the GUI singleton) ──────────────────────────────────────────
    println!("Client:");
    println!(
        "  Frontend: {}",
        GUI_PROCESS.unwrap_or("<unsupported on this platform>")
    );
    if pids.is_empty() {
        println!("  Process:  not running");
    } else {
        let list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        println!("  Process:  running (PID {list})");
    }
    match (&daemon, &gui_conn) {
        (Some(_), Some(c)) => {
            println!("  Build:    {}", build_display(&c.build, &c.build_profile));
            println!("  Attached: {} ({})", c.label, c.transport);
        }
        (Some(_), None) if !pids.is_empty() => {
            println!("  Build:    <unknown — GUI running but not attached to the local daemon>");
        }
        (Some(_), None) => println!("  Build:    <none — no GUI client attached>"),
        (None, _) => println!("  Build:    <unknown — local daemon not running>"),
    }

    // ── Daemon ──────────────────────────────────────────────────────────────
    println!("Daemon:");
    match &daemon {
        Some(d) => {
            let dprofile = d.build_profile.map(|p| p.as_str()).unwrap_or("<unknown>");
            println!("  Build:    {}", build_display(&d.kmuxd_build, dprofile));
            println!("  Version:  {}", d.kmuxd_version);
            println!(
                "  Protocol: {}",
                d.protocol_range.map_or_else(
                    || format!("legacy-{}", d.protocol_version),
                    |range| range.to_string()
                )
            );
            println!("  PID:      {}", d.pid);
        }
        None => println!("  Status:   not running"),
    }

    // ── This CLI ────────────────────────────────────────────────────────────
    println!("CLI:");
    println!("  Build:    {}", build_display(&cli_build, cli_profile));
    println!("  Protocol: {PROTOCOL_RANGE}");

    // ── Skew warnings ───────────────────────────────────────────────────────
    // Every comparison is classified through `kmux_protocol::compat`, the same
    // SSoT the attach gate and `kmux daemon status` use.
    use kmux_protocol::compat::{self, Match3};
    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if let Some(d) = &daemon {
        // The most consequential skew: a protocol gap means the CLI/GUI cannot
        // even connect to this daemon.
        if compat::protocol_match(d.protocol_range) != Match3::Same {
            warnings.push(format!(
                "CLI protocol ({PROTOCOL_RANGE}) differs from the daemon ({}) — they cannot \
                 connect. Run `kmux daemon restart` to update the daemon, or reinstall kmux.",
                d.protocol_range.map_or_else(
                    || format!("legacy-{}", d.protocol_version),
                    |range| range.to_string()
                )
            ));
        }
        if compat::profile_match(d.build_profile) != Match3::Same {
            warnings.push(format!(
                "CLI profile ({}) differs from the daemon ({}).",
                BuildProfile::CURRENT,
                d.build_profile.map(|p| p.as_str()).unwrap_or("<unknown>"),
            ));
        }
        match compat::build_match(&d.kmuxd_build, &cli_build) {
            Match3::Unknown => notes.push(
                "daemon build is unknown (it predates build reporting); reinstall/restart it to \
                 enable build-skew detection."
                    .to_string(),
            ),
            // The skew that hides in plain sight: the GUI launches the current
            // install while `kmux …` may run a stale CLI.
            Match3::Differ => warnings.push(format!(
                "CLI build ({cli_build}) differs from the daemon ({}). Reinstall kmux so the \
                 CLI matches.",
                d.kmuxd_build
            )),
            Match3::Same => {}
        }
        if let Some(c) = &gui_conn
            && !d.kmuxd_build.is_empty()
            && compat::build_match(&c.build, &d.kmuxd_build) == Match3::Differ
        {
            warnings.push(format!(
                "GUI client build ({}) differs from the daemon ({}). Restart it: \
                 `kmux client restart`.",
                c.build, d.kmuxd_build
            ));
        }
    }
    for w in &warnings {
        println!("⚠  {w}");
    }
    for n in &notes {
        println!("ℹ  {n}");
    }
    if daemon.is_some() && warnings.is_empty() && notes.is_empty() {
        println!("✓  client, daemon, and CLI builds all match.");
    }

    Ok(())
}

/// Tail the client log file, optionally following new output — the client-side
/// counterpart of `kmux daemon logs`. The GUI client is a local singleton, so
/// this is always the local file (no remote form, unlike `kmux daemon logs`).
async fn client_logs(follow: bool, lines: Option<usize>) -> anyhow::Result<()> {
    let log_path = kmux_protocol::dirs::client_log_path()?;
    super::logs::tail_local_log(
        &log_path,
        lines,
        follow,
        "Has a kmux client been run at least once?",
    )
    .await
}

/// Stop the running GUI client: SIGTERM each PID, then SIGKILL any that linger.
fn client_stop() {
    let pids = gui_pids();
    if pids.is_empty() {
        println!("No GUI client is running.");
        return;
    }
    for pid in &pids {
        terminate(*pid);
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    println!("Stopped GUI client (PID {list}).");
}

/// SIGTERM a pid, then SIGKILL it if it is still alive after a short grace
/// period. Uses the `kill` command so no extra dependency is needed.
fn terminate(pid: u32) {
    let pid = pid.to_string();
    let _ = std::process::Command::new("kill").arg(&pid).status();
    std::thread::sleep(Duration::from_millis(400));
    if pid_alive(&pid) {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid])
            .status();
    }
}

fn pid_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stop the GUI client, then relaunch it through the launcher entrypoint.
fn client_restart() -> anyhow::Result<()> {
    client_stop();
    // Let the singleton (D-Bus / bundle) lock clear before relaunching.
    std::thread::sleep(Duration::from_millis(400));

    // Spawn the launcher (`current_exe` is the `kmux` front door — or a frontend
    // binary — both of which open the GUI when run with no subcommand). Detach
    // it and clear `KMUX` so the relaunch isn't refused by the nested-kmux guard
    // when this command is run from inside a pane.
    let exe = std::env::current_exe()?;
    std::process::Command::new(&exe)
        .env_remove("KMUX")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to relaunch {}: {e}", exe.display()))?;
    println!("Relaunched GUI client.");
    Ok(())
}
