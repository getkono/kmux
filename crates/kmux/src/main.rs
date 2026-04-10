mod app;
mod config;
mod key_convert;
mod mode;
mod theme;
mod ui;

use std::io;

use clap::{Parser, Subcommand};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use rand::RngCore;
use ratatui::prelude::CrosstermBackend;
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

use app::App;
use kmux_client::token::read_local_token;

#[derive(Parser, Debug)]
#[command(name = "kmux", about = "kmux remote terminal client (TUI)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Remote server host (omit to auto-start and connect to the local daemon)
    server: Option<String>,

    /// Server host (overridden by positional server argument if given)
    #[arg(long)]
    host: Option<String>,

    /// Server port
    #[arg(long)]
    port: Option<u16>,

    /// Auth token (reads from runtime token file if not provided)
    #[arg(long)]
    token: Option<String>,

    /// Accept self-signed / invalid TLS certificates
    #[arg(long)]
    accept_invalid_certs: bool,

    /// Color theme: built-in name (one-dark, catppuccin-latte, catppuccin-frappe,
    /// catppuccin-macchiato, catppuccin-mocha, dracula) or a custom theme name
    /// from ~/.config/kmux/themes/<name>.toml
    #[arg(long)]
    theme: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the local kmux daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    /// Start the daemon in the background
    Start,
    /// Gracefully stop the daemon
    Stop,
    /// Show daemon status (PID, uptime, port, session count)
    Status,
    /// Stop then restart the daemon
    Restart,
    /// Print daemon log file (use -f/--follow to stream new lines)
    Logs {
        /// Follow new log output (like tail -f)
        #[arg(short, long)]
        follow: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let instance_id = generate_instance_id();

    // Log to a persistent file; fall back to stderr if the path can't be opened.
    match kmux_protocol::dirs::client_log_path().and_then(|p| {
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?)
    }) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("kmux=info".parse().unwrap()),
                )
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive("kmux=info".parse().unwrap()),
                )
                .with_writer(std::io::stderr)
                .init();
        }
    }
    tracing::info!(instance_id = %instance_id, "kmux started");

    let cli = Cli::parse();

    // Handle daemon subcommands before any TUI setup.
    if let Some(Command::Daemon { action }) = cli.command {
        return run_daemon_command(action).await;
    }

    // Capture the client's working directory before doing anything else.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default();

    // When no positional server, --host, --port, or --token is given, auto-start
    // (or reuse) a local daemon and retrieve the connection parameters from it.
    let is_local =
        cli.server.is_none() && cli.host.is_none() && cli.port.is_none() && cli.token.is_none();

    let (host, port, token, accept_invalid_certs) = if is_local {
        let status = kmux_client::daemon::ensure_daemon().await?;
        // The daemon uses a self-signed cert, so accept-invalid-certs is implied.
        ("127.0.0.1".to_string(), status.port, status.token, true)
    } else {
        // Positional `server` arg takes precedence over `--host`. It may be
        // "host" or "host:port".
        let (host, port) = if let Some(server) = cli.server {
            if let Some((h, p_str)) = server.rsplit_once(':') {
                let p = p_str.parse().unwrap_or(8443);
                (h.to_string(), cli.port.unwrap_or(p))
            } else {
                (server, cli.port.unwrap_or(8443))
            }
        } else {
            let host = cli.host.unwrap_or_else(|| "127.0.0.1".to_string());
            let port = cli.port.unwrap_or(8443);
            (host, port)
        };
        let token = cli.token.or_else(read_local_token).unwrap_or_default();
        (host, port, token, cli.accept_invalid_certs)
    };

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Install panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    let theme = config::resolve_theme(cli.theme.as_deref());

    let mut app = App::new(
        host,
        port,
        token,
        accept_invalid_certs,
        is_local,
        initial_cwd,
        theme,
        instance_id.clone(),
    );

    let result = app
        .run(&mut terminal)
        .instrument(tracing::info_span!("instance", id = %instance_id))
        .await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn run_daemon_command(action: DaemonAction) -> anyhow::Result<()> {
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
            // Wait briefly for the process to exit.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let status = kmux_client::daemon::ensure_daemon().await?;
            println!(
                "Daemon restarted — PID {}, port {}",
                status.pid, status.port
            );
        }

        DaemonAction::Logs { follow } => {
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

fn format_uptime(secs: u64) -> String {
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

fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
