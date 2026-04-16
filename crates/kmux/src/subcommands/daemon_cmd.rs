use crate::cli::DaemonAction;

use super::format_uptime;

pub async fn run_daemon_command(action: DaemonAction) -> anyhow::Result<()> {
    match action {
        DaemonAction::Start => {
            // Check if already running.
            if let Some(status) = kmux_client::daemon::query_daemon().await {
                println!(
                    "Daemon already running — PID {}, port {}",
                    status.pid, status.port
                );
                return Ok(());
            }
            let status = kmux_client::daemon::ensure_daemon().await?;
            println!("Daemon started — PID {}, port {}", status.pid, status.port);
        }

        DaemonAction::Stop => {
            kmux_client::daemon::stop_daemon().await.map_err(|e| {
                anyhow::anyhow!("Daemon is not running or could not be stopped: {e}")
            })?;
            println!("Daemon stopped");
        }

        DaemonAction::Status => match kmux_client::daemon::query_daemon().await {
            Some(status) => {
                println!("Status:   running");
                println!("PID:      {}", status.pid);
                println!("Port:     {}", status.port);
                println!("Uptime:   {}", format_uptime(status.uptime_secs));
                println!("Sessions: {}", status.session_count);
            }
            None => {
                println!("Status:   not running");
                std::process::exit(1);
            }
        },

        DaemonAction::Restart => {
            // Stop (ignore "not running").
            let _ = kmux_client::daemon::stop_daemon().await;
            // Poll until the old daemon is confirmed dead (up to 3 seconds).
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
            let status = kmux_client::daemon::ensure_daemon().await?;
            println!(
                "Daemon restarted — PID {}, port {}",
                status.pid, status.port
            );
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
