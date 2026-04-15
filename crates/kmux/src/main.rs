mod app;
mod config;
mod host_caps;
mod key_convert;
mod mode;
mod theme;
mod ui;

use std::io;

use clap::{Args, Parser, Subcommand, ValueEnum};
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
use kmux_client::ssh::{self, ParsedServer, RemoteTarget, SshSession};
use kmux_client::token::read_local_token;

#[derive(Parser, Debug)]
#[command(name = "kmux", about = "kmux remote terminal client (TUI)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    connect: ConnectArgs,

    /// Color theme: built-in name (one-dark, catppuccin-latte, catppuccin-frappe,
    /// catppuccin-macchiato, catppuccin-mocha, dracula) or a custom theme name
    /// from ~/.config/kmux/themes/<name>.toml
    #[arg(long, global = true)]
    theme: Option<String>,
}

/// Arguments for connecting to a server (the default action).
#[derive(Args, Debug)]
struct ConnectArgs {
    /// Remote server: user@host, user@host:/path, user@host:port, alias
    /// (omit to auto-start and connect to the local daemon)
    server: Option<String>,

    /// Auto-attach to a named session (by display name or word_id)
    #[arg(short, long)]
    session: Option<String>,

    /// Working directory for a new session (used with --session or user@host:/path)
    #[arg(long)]
    cwd: Option<String>,

    /// SSH port to use when connecting to a remote target (overrides hosts.toml)
    #[arg(long)]
    ssh_port: Option<u16>,

    // ── Hidden legacy/advanced flags ─────────────────────────────────────────
    /// Server host (prefer positional server argument)
    #[arg(long, hide = true)]
    host: Option<String>,

    /// Server port (prefer user@host:port or host:port syntax)
    #[arg(long, hide = true)]
    port: Option<u16>,

    /// Auth token (reads from runtime token file if not provided)
    #[arg(long, hide = true)]
    token: Option<String>,

    /// Skip SSH tunneling; connect directly via QUIC
    #[arg(long, hide = true)]
    no_ssh: bool,

    /// Accept self-signed / invalid TLS certificates
    #[arg(long, hide = true)]
    accept_invalid_certs: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Manage the local kmux daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// List sessions on a server without launching the TUI
    #[command(alias = "ls")]
    ListSessions {
        /// Remote server (user@host, alias from hosts.toml; omit for local daemon)
        server: Option<String>,

        /// SSH port override
        #[arg(long)]
        ssh_port: Option<u16>,

        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,

        // Hidden advanced flags for list-sessions
        #[arg(long, hide = true)]
        host: Option<String>,
        #[arg(long, hide = true)]
        port: Option<u16>,
        #[arg(long, hide = true)]
        token: Option<String>,
        #[arg(long, hide = true)]
        no_ssh: bool,
        #[arg(long, hide = true)]
        accept_invalid_certs: bool,
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

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

/// Resolved connection parameters ready for use.
struct ResolvedConnection {
    host: String,
    port: u16,
    /// TCP port for headless commands (list-sessions). Falls back to `port` if unset.
    tcp_port: Option<u16>,
    token: String,
    accept_invalid_certs: bool,
    is_local: bool,
    ssh_session: Option<SshSession>,
    ssh_target: Option<RemoteTarget>,
    parsed_server: Option<ParsedServer>,
}

/// Resolve connection parameters from CLI arguments.
///
/// Handles three modes: local daemon, SSH negotiation, or direct QUIC.
async fn resolve_connection(
    server: Option<&str>,
    ssh_port_override: Option<u16>,
    no_ssh: bool,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    accept_invalid_certs: bool,
) -> anyhow::Result<ResolvedConnection> {
    let is_local = server.is_none()
        && host_override.is_none()
        && port_override.is_none()
        && token_override.is_none();

    let parsed = server.map(ssh::parse_server_string);

    // Detect SSH mode: server has a user or matches a hosts.toml alias with a user,
    // and --no-ssh is not given.
    let ssh_target = if !no_ssh {
        parsed
            .as_ref()
            .and_then(ssh::resolve_remote_target)
            .map(|mut t| {
                if let Some(p) = ssh_port_override {
                    t.ssh_port = Some(p);
                }
                t
            })
    } else {
        None
    };

    if let Some(target) = ssh_target {
        tracing::info!(
            host = %target.host,
            user = ?target.user,
            "SSH negotiation starting"
        );
        match ssh::negotiate(&target).await {
            Ok(session) => {
                let host = "127.0.0.1".to_string();
                let port = session.local_tcp_port;
                let token = session.token.clone();
                Ok(ResolvedConnection {
                    host,
                    port,
                    tcp_port: None,
                    token,
                    accept_invalid_certs: true,
                    is_local: false,
                    ssh_session: Some(session),
                    ssh_target: Some(target),
                    parsed_server: parsed,
                })
            }
            Err(e) => {
                eprintln!("SSH negotiation failed: {e}");
                std::process::exit(1);
            }
        }
    } else if is_local {
        let status = kmux_client::daemon::ensure_daemon().await?;
        Ok(ResolvedConnection {
            host: "127.0.0.1".to_string(),
            port: status.port,
            tcp_port: Some(status.tcp_port),
            token: status.token,
            accept_invalid_certs: true,
            is_local: true,
            ssh_session: None,
            ssh_target: None,
            parsed_server: parsed,
        })
    } else {
        // Direct QUIC: positional server (host:port) or explicit --host/--port.
        let (host, port) = if let Some(ref parsed) = parsed {
            (
                parsed.host.clone(),
                port_override.or(parsed.port).unwrap_or(8443),
            )
        } else {
            let host = host_override
                .map(|s| s.to_string())
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = port_override.unwrap_or(8443);
            (host, port)
        };
        let token = token_override
            .map(|s| s.to_string())
            .or_else(read_local_token)
            .unwrap_or_default();
        Ok(ResolvedConnection {
            host,
            port,
            tcp_port: None,
            token,
            accept_invalid_certs,
            is_local: false,
            ssh_session: None,
            ssh_target: None,
            parsed_server: parsed,
        })
    }
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

    // Handle subcommands before any TUI setup.
    match cli.command {
        Some(Command::Daemon { action }) => return run_daemon_command(action).await,
        Some(Command::ListSessions {
            server,
            ssh_port,
            format,
            host,
            port,
            token,
            no_ssh,
            accept_invalid_certs,
        }) => {
            return run_list_sessions(
                server.as_deref(),
                ssh_port,
                format,
                host.as_deref(),
                port,
                token.as_deref(),
                no_ssh,
                accept_invalid_certs,
            )
            .await;
        }
        None => {}
    }

    // ── Default: connect and launch TUI ─────────���────────────────────────────

    // Capture the client's working directory before doing anything else.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let conn = resolve_connection(
        cli.connect.server.as_deref(),
        cli.connect.ssh_port,
        cli.connect.no_ssh,
        cli.connect.host.as_deref(),
        cli.connect.port,
        cli.connect.token.as_deref(),
        cli.connect.accept_invalid_certs,
    )
    .await?;

    // Compute effective cwd: explicit --cwd > :path from server string > local cwd
    let auto_cwd = cli
        .connect
        .cwd
        .or_else(|| conn.parsed_server.as_ref().and_then(|p| p.path.clone()));

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
        conn.host,
        conn.port,
        conn.token,
        conn.accept_invalid_certs,
        conn.is_local,
        initial_cwd,
        theme,
        instance_id.clone(),
        conn.ssh_session,
        conn.ssh_target,
        cli.connect.session,
        auto_cwd,
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

#[allow(clippy::too_many_arguments)]
async fn run_list_sessions(
    server: Option<&str>,
    ssh_port: Option<u16>,
    format: OutputFormat,
    host_override: Option<&str>,
    port_override: Option<u16>,
    token_override: Option<&str>,
    no_ssh: bool,
    accept_invalid_certs: bool,
) -> anyhow::Result<()> {
    let conn = resolve_connection(
        server,
        ssh_port,
        no_ssh,
        host_override,
        port_override,
        token_override,
        accept_invalid_certs,
    )
    .await?;

    // Connect headlessly via TCP, send auth + SessionList, print results.
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, PROTOCOL_VERSION, ServerMessage,
    };
    use kmux_protocol::{decode_server, encode_client, read_frame, write_frame};
    use tokio::net::TcpStream;

    let tcp_port = conn.tcp_port.unwrap_or(conn.port);
    let stream = TcpStream::connect(format!("{}:{}", conn.host, tcp_port))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {}:{}: {e}", conn.host, tcp_port))?;

    let (mut read_half, mut write_half) = stream.into_split();

    // Authenticate.
    let auth_msg = ClientMessage::Auth {
        token: conn.token,
        protocol_version: PROTOCOL_VERSION,
        capabilities: ClientCapabilities::default(),
        connection_id: None,
    };
    let auth_bytes = encode_client(&auth_msg)?;
    write_frame(&mut write_half, &auth_bytes).await?;

    // Wait for AuthResult.
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before auth response"))?;
        let msg = decode_server(&data)?;
        match msg {
            ServerMessage::AuthResult {
                success: true,
                client_id,
                ..
            } => {
                tracing::debug!(?client_id, "Authenticated for list-sessions");
                break;
            }
            ServerMessage::AuthResult {
                success: false,
                reason,
                ..
            } => {
                anyhow::bail!(
                    "Authentication failed: {}",
                    reason.unwrap_or_else(|| "unknown error".into())
                );
            }
            _ => continue,
        }
    }

    // Request session list.
    let list_msg = ClientMessage::SessionList { request_id: 1 };
    let list_bytes = encode_client(&list_msg)?;
    write_frame(&mut write_half, &list_bytes).await?;

    // Wait for SessionListResult.
    loop {
        let data = read_frame(&mut read_half)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Connection closed before session list"))?;
        let msg = decode_server(&data)?;
        match msg {
            ServerMessage::SessionListResult { sessions, .. } => {
                print_sessions(&sessions, &format);
                return Ok(());
            }
            _ => continue,
        }
    }
}

fn print_sessions(sessions: &[kmux_protocol::messages::SessionEntry], format: &OutputFormat) {
    match format {
        OutputFormat::Table => {
            if sessions.is_empty() {
                println!("No active sessions");
                return;
            }
            println!("{:<16} {:<10} {:<40} {:<6}", "NAME", "ID", "CWD", "PANES");
            for entry in sessions {
                println!(
                    "{:<16} {:<10} {:<40} {:<6}",
                    entry.meta.name,
                    entry.meta.word_id,
                    entry.meta.cwd,
                    entry.panes.len(),
                );
            }
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(sessions).expect("sessions are serializable");
            println!("{json}");
        }
    }
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
