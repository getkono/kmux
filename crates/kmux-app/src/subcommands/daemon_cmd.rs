use crate::cli::{DaemonAction, OutputFormat};

use super::render;

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

        DaemonAction::Stop => {
            kmux_client::daemon::stop_daemon().await.map_err(|e| {
                anyhow::anyhow!("Daemon is not running or could not be stopped: {e}")
            })?;
            println!("Daemon stopped");
        }

        DaemonAction::Status => {
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
                        status.protocol_version != 0 && status.protocol_version != PROTOCOL_VERSION;
                    let profile_mismatch = status.build_profile != Some(BuildProfile::CURRENT);

                    println!("Status:   running");
                    println!("Socket:   {socket_display}");
                    println!("PID:      {}", status.pid);
                    println!("Port:     {}", status.port);
                    println!("Uptime:   {}", render::format_uptime(status.uptime_secs));
                    println!("Sessions: {}", status.session_count);
                    println!("Protocol: {}", status.protocol_version);
                    println!("Version:  {}", status.kmuxd_version);
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
                                "timed out waiting for the successor daemon to take over"
                            );
                        }
                    }
                }
                Ok(false) => {
                    anyhow::bail!("a restart is already in progress");
                }
                Err(_) => {
                    // Old daemon predates graceful restart: hard restart (running
                    // shells do not survive this one-time fallback).
                    let _ = kmux_client::daemon::stop_daemon().await;
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
                    loop {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if kmux_client::daemon::query_daemon().await.is_none() {
                            break;
                        }
                        if tokio::time::Instant::now() >= deadline {
                            anyhow::bail!("timed out waiting for daemon to stop");
                        }
                    }
                    let status = kmux_client::daemon::ensure_compatible_daemon().await?;
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
