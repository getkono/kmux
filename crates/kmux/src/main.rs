mod app;
mod key_convert;
mod mode;
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
use tracing_subscriber::EnvFilter;

use app::App;
use kmux_client::token::read_local_token;

#[derive(Parser, Debug)]
#[command(name = "kmux", about = "kmux remote terminal client (TUI)")]
struct Cli {
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("kmux=info".parse().unwrap()))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

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

    let mut app = App::new(
        host,
        port,
        token,
        accept_invalid_certs,
        is_local,
        initial_cwd,
    );

    let result = app.run(&mut terminal).await;

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
