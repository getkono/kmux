mod app;
mod cli;
mod config;
mod host_caps;
mod key_convert;
mod mode;
mod recent_servers;
mod subcommands;
mod theme;
mod ui;

use std::io;

use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::prelude::CrosstermBackend;
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

use app::App;
use cli::{Cli, Command, generate_instance_id};
use subcommands::{resolve_connection, run_daemon_command, run_list_sessions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    async_main().await
}

async fn async_main() -> anyhow::Result<()> {
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
    tracing::info!(
        instance_id = %instance_id,
        version = env!("CARGO_PKG_VERSION"),
        protocol_version = kmux_protocol::messages::PROTOCOL_VERSION,
        "kmux started"
    );

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

    // ── Default: connect and launch TUI ──────────────────────────────────────

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
