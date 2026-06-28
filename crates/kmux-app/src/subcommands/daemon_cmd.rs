use std::time::Duration;

use crate::cli::{DaemonAction, OutputFormat};

use super::render;

/// How long to wait for the daemon process to exit after a graceful `stop`
/// before treating it as stuck. Generous because a clean shutdown checkpoints
/// session state, which can be slow on a loaded or near-full disk.
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// Grace period between `SIGTERM` and `SIGKILL` when force-killing an
/// unresponsive daemon.
const FORCE_KILL_GRACE: Duration = Duration::from_secs(2);

/// Upper bound on the best-effort process-overview query that enriches the
/// stop summary. Kept short so a wedged data plane degrades the summary rather
/// than stalling the stop.
const PROCESS_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn run_daemon_command(action: DaemonAction) -> anyhow::Result<()> {
    match action {
        DaemonAction::Start => {
            // Check if already running.
            if let Some(status) = kmux_client::daemon::query_daemon().await {
                use kmux_protocol::messages::PROTOCOL_VERSION;
                if status.protocol_version != 0 && status.protocol_version != PROTOCOL_VERSION {
                    anyhow::bail!(
                        "Daemon is running (PID {}) with protocol version {} but this client \
                         uses {}. Run `kmux daemon restart` to restart it.",
                        status.pid,
                        status.protocol_version,
                        PROTOCOL_VERSION
                    );
                }
                println!(
                    "Daemon already running — PID {}, port {}",
                    status.pid, status.port
                );
                return Ok(());
            }
            let status = kmux_client::daemon::ensure_compatible_daemon().await?;
            println!("Daemon started — PID {}, port {}", status.pid, status.port);
        }

        DaemonAction::Stop { yes, force } => {
            run_daemon_stop(yes, force).await?;
        }

        DaemonAction::Status => {
            use kmux_protocol::compat::{self, Match3};
            use kmux_protocol::dirs::BuildProfile;
            use kmux_protocol::messages::PROTOCOL_VERSION;

            let socket_display = kmux_protocol::dirs::socket_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|e| format!("<error: {e}>"));

            match kmux_client::daemon::query_daemon().await {
                Some(status) => {
                    let daemon_profile = status
                        .build_profile
                        .map(|p| p.as_str())
                        .unwrap_or("<unknown>");
                    let protocol_mismatch =
                        compat::protocol_match(status.protocol_version) == Match3::Differ;
                    let profile_mismatch =
                        compat::profile_match(status.build_profile) != Match3::Same;

                    println!("Status:   running");
                    println!("Socket:   {socket_display}");
                    println!("PID:      {}", status.pid);
                    println!("Port:     {}", status.port);
                    println!("Uptime:   {}", render::format_uptime(status.uptime_secs));
                    println!("Sessions: {}", status.session_count);
                    println!("Protocol: {}", status.protocol_version);
                    println!("Version:  {}", status.kmuxd_version);
                    if !status.kmuxd_build.is_empty() {
                        println!("Build:    {}", status.kmuxd_build);
                    }
                    println!(
                        "Profile:  daemon={daemon_profile} client={client}",
                        client = BuildProfile::CURRENT,
                    );
                    if protocol_mismatch {
                        println!(
                            "Error:    protocol version mismatch (client={PROTOCOL_VERSION}). \
                             Run `kmux daemon restart`."
                        );
                    }
                    if profile_mismatch {
                        println!(
                            "Error:    build profile mismatch — kmux refuses to attach. \
                             Debug and release builds use separate runtime dirs; run the \
                             matching `kmux` binary or restart the daemon with a matching build."
                        );
                    }
                    if protocol_mismatch || profile_mismatch {
                        std::process::exit(1);
                    }
                }
                None => {
                    println!("Status:   not running");
                    println!("Socket:   {socket_display}");
                    println!("Profile:  client={}", BuildProfile::CURRENT);
                    std::process::exit(1);
                }
            }
        }

        DaemonAction::Restart => {
            // If nothing is running, this is just a start.
            let Some(old) = kmux_client::daemon::query_daemon().await else {
                let status = kmux_client::daemon::ensure_compatible_daemon().await?;
                println!("Daemon started — PID {}, port {}", status.pid, status.port);
                return Ok(());
            };
            let old_pid = old.pid;

            // Prefer a graceful live handoff so running shells survive. Fall back
            // to a hard stop-then-respawn against a daemon too old to support it.
            match kmux_client::daemon::restart_daemon().await {
                Ok(true) => {
                    // Wait for the successor (a distinct PID) to take over.
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        if let Some(s) = kmux_client::daemon::query_daemon().await
                            && s.pid != old_pid
                        {
                            println!(
                                "Daemon restarted (live handoff) — PID {}, port {}",
                                s.pid, s.port
                            );
                            return Ok(());
                        }
                        if tokio::time::Instant::now() >= deadline {
                            anyhow::bail!(
                                "timed out waiting for the successor daemon to take over. \
                                 The previous daemon (PID {old_pid}) kept serving; running \
                                 shells are intact.{}",
                                kmux_client::daemon::boot_log_hint()
                            );
                        }
                    }
                }
                Ok(false) => {
                    anyhow::bail!("a restart is already in progress");
                }
                Err(_) => {
                    // Old daemon predates graceful restart: hard restart (running
                    // shells do not survive this one-time fallback). Verify the
                    // old process is actually gone before respawning, by PID.
                    let _ = kmux_client::daemon::stop_daemon().await;
                    if !kmux_client::daemon::wait_for_exit(old_pid, GRACEFUL_STOP_TIMEOUT).await {
                        anyhow::bail!(
                            "timed out waiting for daemon (PID {old_pid}) to stop; \
                             run `kmux daemon stop --force` to terminate it"
                        );
                    }
                    let status = kmux_client::daemon::ensure_compatible_daemon()
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("{e}{}", kmux_client::daemon::boot_log_hint())
                        })?;
                    println!(
                        "Daemon restarted — PID {}, port {}",
                        status.pid, status.port
                    );
                }
            }
        }

        DaemonAction::Sessions { all, format } => {
            match kmux_client::daemon::query_daemon_sessions().await {
                Ok(resp) => match format {
                    OutputFormat::Json => render::render_json(&resp),
                    OutputFormat::Table => {
                        let rows = render::daemon_session_rows(&resp, all);
                        render::render(&rows, &format, "No active connections");
                    }
                },
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }

        DaemonAction::Logs { follow } => {
            use std::io;

            let log_path = kmux_protocol::dirs::daemon_log_path()?;
            if !log_path.exists() {
                eprintln!(
                    "Log file not found: {}\nHas the daemon been run at least once?",
                    log_path.display()
                );
                std::process::exit(1);
            }

            use tokio::io::{AsyncReadExt, AsyncSeekExt};
            let mut file = tokio::fs::File::open(&log_path).await?;

            // Print all existing content.
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).await?;
            io::Write::write_all(&mut io::stdout(), &buf)?;

            if follow {
                // Seek to end and poll for new bytes.
                file.seek(std::io::SeekFrom::End(0)).await?;
                let mut read_buf = vec![0u8; 4096];
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let n = file.read(&mut read_buf).await?;
                    if n > 0 {
                        io::Write::write_all(&mut io::stdout(), &read_buf[..n])?;
                        io::Write::flush(&mut io::stdout())?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Stop the local daemon, *proving* it exited rather than trusting its reply.
///
/// Resolves into one of three states:
/// 1. **Responsive** — show a summary, confirm, send a graceful `stop`, then
///    poll the PID until it dies; on timeout, escalate to `SIGKILL`.
/// 2. **Unresponsive** — the control socket is wedged but a live PID owns the
///    pid file; offer an OS force-kill (`SIGTERM`→`SIGKILL`).
/// 3. **Not running** — say so and exit cleanly.
///
/// `yes` skips the initial confirmation; `force` skips all prompts and
/// force-kills when a graceful stop will not exit. With neither flag and no
/// TTY, the command refuses rather than guess.
async fn run_daemon_stop(yes: bool, force: bool) -> anyhow::Result<()> {
    use std::io::IsTerminal;

    match kmux_client::daemon::query_daemon().await {
        // ── Responsive: graceful stop with a verified exit ──────────────────
        Some(status) => {
            // Summary: sessions + clients from the control socket, enriched with
            // best-effort per-pane processes from the data plane.
            let processes = collect_processes_by_session(&status).await;
            match kmux_client::daemon::query_daemon_sessions().await {
                Ok(resp) => {
                    println!(
                        "Daemon PID {} — up {}, {} session(s)",
                        status.pid,
                        render::format_uptime(status.uptime_secs),
                        resp.sessions.len(),
                    );
                    let rows = render::stop_summary_rows(&resp, &processes);
                    render::render(&rows, &OutputFormat::Table, "  (no active sessions)");
                    println!();
                }
                Err(e) => {
                    // The summary is best-effort; never block a stop on it.
                    println!(
                        "Daemon PID {} — up {}, {} session(s)",
                        status.pid,
                        render::format_uptime(status.uptime_secs),
                        status.session_count,
                    );
                    eprintln!("warning: could not read session summary: {e}");
                }
            }

            if !yes && !force {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "refusing to stop the daemon non-interactively; \
                         re-run with --yes (graceful) or --force"
                    );
                }
                if !super::confirm_yes_no("Stop this daemon?")? {
                    println!("Cancelled.");
                    return Ok(());
                }
            }

            // Ask for a graceful shutdown, then confirm the process is gone.
            kmux_client::daemon::stop_daemon()
                .await
                .map_err(|e| anyhow::anyhow!("failed to send stop command: {e}"))?;
            if kmux_client::daemon::wait_for_exit(status.pid, GRACEFUL_STOP_TIMEOUT).await {
                println!("Daemon stopped (PID {}).", status.pid);
                return Ok(());
            }

            // It accepted the request but did not exit — escalate.
            eprintln!(
                "Daemon (PID {}) did not exit within {}s of a graceful stop.",
                status.pid,
                GRACEFUL_STOP_TIMEOUT.as_secs(),
            );
            let escalate = force
                || (std::io::stdin().is_terminal()
                    && super::confirm_yes_no(
                        "Force-kill it (SIGKILL)? Unsaved work in its panes will be lost.",
                    )?);
            if !escalate {
                anyhow::bail!("Daemon left running (PID {}).", status.pid);
            }
            // The graceful request was already sent and ignored, so a second
            // SIGTERM is pointless — go straight to SIGKILL.
            kmux_client::daemon::force_kill_daemon(status.pid, false, FORCE_KILL_GRACE).await?;
            println!("Daemon force-killed (PID {}).", status.pid);
            Ok(())
        }

        // ── Unresponsive (live PID, wedged socket) or simply not running ────
        None => match kmux_client::daemon::running_daemon_pid() {
            Some(pid) => {
                eprintln!("Daemon (PID {pid}) is not responding on its control socket.");
                let interactive = std::io::stdin().is_terminal();
                let escalate = force
                    || (interactive
                        && super::confirm_yes_no("Force-kill the unresponsive daemon?")?);
                if !escalate {
                    if !interactive {
                        anyhow::bail!(
                            "daemon is unresponsive; re-run with --force to terminate it"
                        );
                    }
                    anyhow::bail!("Daemon left running (PID {pid}).");
                }
                // Unresponsive: try a clean SIGTERM first, then SIGKILL.
                kmux_client::daemon::force_kill_daemon(pid, true, FORCE_KILL_GRACE).await?;
                println!("Unresponsive daemon force-killed (PID {pid}).");
                Ok(())
            }
            None => {
                println!("Daemon is not running.");
                Ok(())
            }
        },
    }
}

/// Best-effort: map each session word-id to the distinct process names running
/// across its panes.
///
/// Connects to the data plane using the params already in `status` — pointedly
/// *not* via `resolve_connection`/`ensure_compatible_daemon`, so a stop can
/// never accidentally *spawn* a daemon. Returns an empty map (RUNNING shows `-`)
/// on any failure or timeout; the summary stays useful without it.
async fn collect_processes_by_session(
    status: &kmux_client::daemon::DaemonStatus,
) -> std::collections::HashMap<String, Vec<String>> {
    use std::collections::HashMap;

    let panes =
        match tokio::time::timeout(PROCESS_QUERY_TIMEOUT, query_pane_processes(status)).await {
            Ok(Ok(panes)) => panes,
            _ => return HashMap::new(),
        };

    let mut by_session: HashMap<String, Vec<String>> = HashMap::new();
    for pane in panes {
        // pane_id is "{word_id}/{pane_index}" — the prefix joins to the session.
        let Some(word_id) = pane.pane_id.split('/').next().filter(|w| !w.is_empty()) else {
            continue;
        };
        let names = by_session.entry(word_id.to_string()).or_default();
        for proc in pane.processes {
            if !names.contains(&proc.name) {
                names.push(proc.name);
            }
        }
    }
    by_session
}

/// One-shot data-plane query for the process overview, using `status`'s
/// connection params directly (never spawning a daemon).
async fn query_pane_processes(
    status: &kmux_client::daemon::DaemonStatus,
) -> anyhow::Result<Vec<kmux_protocol::messages::PaneProcesses>> {
    use kmux_protocol::messages::{ClientMessage, ServerMessage};
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
    use tokio::net::TcpStream;

    let tcp_port = if status.tcp_port != 0 {
        status.tcp_port
    } else {
        status.port
    };
    let stream = TcpStream::connect(("127.0.0.1", tcp_port)).await?;
    let (mut read_half, mut write_half) = stream.into_split();

    super::authenticate(&mut read_half, &mut write_half, status.token.clone()).await?;

    write_frame(
        &mut write_half,
        &encode_client(&ClientMessage::ProcessOverview { request_id: 1 })?,
    )
    .await?;
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("connection closed before process overview"))?;
        if let ServerMessage::ProcessOverviewResult { panes, .. } = decode_server(&data)? {
            return Ok(panes);
        }
    }
}
