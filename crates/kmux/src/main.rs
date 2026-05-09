mod app;
mod cli;
mod cmd;
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
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::prelude::CrosstermBackend;
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

use app::App;
use cli::{Cli, Command};
use kmux_client::generate_instance_id;
use subcommands::{
    ListSessionsConfig, parse_target, run_daemon_command, run_dry_run, run_list_sessions,
};

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
        version = concat!(
            env!("CARGO_PKG_VERSION"),
            " (",
            env!("BUILD_GIT_SHA"),
            env!("BUILD_GIT_DIRTY_SUFFIX"),
            ", ",
            env!("BUILD_DATE"),
            ", ",
            env!("BUILD_PROFILE"),
            ")"
        ),
        protocol_version = kmux_protocol::messages::PROTOCOL_VERSION,
        "kmux started"
    );

    let cli = Cli::parse();

    // Handle subcommands before any TUI setup.
    match cli.command {
        Some(Command::Daemon { action }) => return run_daemon_command(action).await,
        Some(Command::ListSessions {
            server_args,
            format,
        }) => {
            return run_list_sessions(ListSessionsConfig {
                server: server_args.server.as_deref(),
                ssh_port: server_args.ssh_port,
                format,
                host_override: server_args.host.as_deref(),
                port_override: server_args.port,
                token_override: server_args.token.as_deref(),
                no_ssh: server_args.no_ssh,
                accept_invalid_certs: server_args.accept_invalid_certs,
            })
            .await;
        }
        None => {}
    }

    // Diagnostic modes short-circuit TUI setup entirely.
    if cli.connect.dry_run && cli.connect.test {
        eprintln!("warning: --test implies --dry-run; running in --test mode.");
    }
    if cli.connect.dry_run || cli.connect.test {
        return run_dry_run(&cli.connect.server_args, cli.connect.test).await;
    }

    // ── Default: connect and launch TUI ──────────────────────────────────────

    // Capture the client's working directory before doing anything else.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_default();

    let (target, parsed_server) = parse_target(
        cli.connect.server_args.server.as_deref(),
        cli.connect.server_args.ssh_port,
        cli.connect.server_args.no_ssh,
        cli.connect.server_args.host.as_deref(),
        cli.connect.server_args.port,
        cli.connect.server_args.token.as_deref(),
        cli.connect.server_args.accept_invalid_certs,
    );

    // Compute effective cwd: explicit --cwd > :path from server string > local cwd
    let auto_cwd = cli
        .connect
        .cwd
        .or_else(|| parsed_server.as_ref().and_then(|p| p.path.clone()));

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Try to enable the kitty keyboard protocol on the host terminal so
    // crossterm sees Shift+Enter, Alt+Enter, Shift+Tab, etc. as
    // distinguishable events rather than collapsing them into the bare
    // key.  Terminals that don't support it ignore the push and we fall
    // back to legacy behaviour.
    let kitty_kbd_supported = supports_keyboard_enhancement().unwrap_or(false);
    if kitty_kbd_supported {
        // Disambiguate is essential.  Alternate keys help kitty-aware apps.
        // We deliberately do NOT enable REPORT_EVENT_TYPES (release events
        // would double-fire keystrokes) or REPORT_ALL_KEYS_AS_ESCAPE_CODES
        // (would break plain typing in legacy code paths).
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
            )
        );
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Install panic hook to restore terminal — pop kitty flags BEFORE leaving
    // the alt screen so the host terminal returns to its baseline state.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        if kitty_kbd_supported {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    let theme = config::resolve_theme(cli.theme.as_deref());

    let mut app = App::new(
        target,
        initial_cwd,
        theme,
        instance_id.clone(),
        cli.connect.session,
        auto_cwd,
        kitty_kbd_supported,
    );

    let result = app
        .run(&mut terminal)
        .instrument(tracing::info_span!("instance", id = %instance_id))
        .await;

    // Restore terminal
    disable_raw_mode()?;
    if kitty_kbd_supported {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // The TUI alternate-screen overlay swallows error text on exit. If the
    // bootstrap (e.g. SSH negotiation) failed, the App stashed the full
    // multi-line diagnostic for us to surface here, after raw-mode is off.
    // Without this, the user only sees the brief disconnect badge and has
    // to dig in `~/.local/state/kmux/client.log` to find out what happened.
    if let Some(err) = app.last_exit_error.take() {
        eprintln!("kmux: connection failed:\n{err}");
    }

    result
}
