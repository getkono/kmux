//! Shared CLI front door for the kmux frontends.
//!
//! Both binaries — `kmux` (GUI) and `kmux-tui` (TUI) — call [`run_cli`]: it
//! initializes logging, parses the CLI, runs any non-interactive subcommand
//! (`ls`, `daemon`, `--dry-run`), or returns a frontend-agnostic [`Plan`]
//! describing the interactive session to launch. Each frontend then builds its
//! own `AppCore` from the plan (supplying its own capabilities) and runs.

use clap::Parser;
use tracing_subscriber::EnvFilter;

use kmux_client::pipeline::ResolvedTarget;

use crate::cli::{Cli, Command};
use crate::config;
use crate::subcommands::{
    ListSessionsConfig, parse_target, run_daemon_command, run_dry_run, run_list_sessions,
};
use crate::theme::Theme;

/// Outcome of [`run_cli`].
pub enum Launch {
    /// A non-interactive subcommand handled everything; the process should exit.
    Done,
    /// Launch an interactive session with these parameters.
    Interactive(Plan),
}

/// Frontend-agnostic interactive launch parameters. Each frontend builds its own
/// `AppCore` (with frontend-specific capabilities) from this.
pub struct Plan {
    pub target: ResolvedTarget,
    pub initial_cwd: String,
    pub auto_cwd: Option<String>,
    pub auto_session: Option<String>,
    pub theme: Theme,
    pub instance_id: String,
}

/// Initialize tracing to the client log file (falling back to stderr) and log a
/// startup line with the build/protocol versions.
pub fn init_logging(instance_id: &str) {
    let filter = || EnvFilter::from_default_env().add_directive("kmux=info".parse().unwrap());
    match kmux_protocol::dirs::client_log_path().and_then(|p| {
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?)
    }) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
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
}

/// Initialize logging, parse the CLI, run any non-interactive subcommand, and
/// otherwise return the interactive launch plan.
pub async fn run_cli(instance_id: String) -> anyhow::Result<Launch> {
    init_logging(&instance_id);

    let cli = Cli::parse();

    // Non-interactive subcommands short-circuit before any frontend setup.
    match cli.command {
        Some(Command::Daemon { action }) => {
            run_daemon_command(action).await?;
            return Ok(Launch::Done);
        }
        Some(Command::ListSessions {
            server_args,
            format,
        }) => {
            run_list_sessions(ListSessionsConfig {
                server: server_args.server.as_deref(),
                ssh_port: server_args.ssh_port,
                format,
            })
            .await?;
            return Ok(Launch::Done);
        }
        None => {}
    }

    if cli.connect.dry_run && cli.connect.test {
        eprintln!("warning: --test implies --dry-run; running in --test mode.");
    }
    if cli.connect.dry_run || cli.connect.test {
        run_dry_run(&cli.connect.server_args, cli.connect.test).await?;
        return Ok(Launch::Done);
    }

    // Interactive: resolve the connection target, cwd, and theme into a plan.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();
    let (target, parsed_server) = parse_target(
        cli.connect.server_args.server.as_deref(),
        cli.connect.server_args.ssh_port,
    );
    let auto_cwd = cli
        .connect
        .cwd
        .or_else(|| parsed_server.as_ref().and_then(|p| p.path.clone()));
    let theme = config::resolve_theme(cli.theme.as_deref());

    Ok(Launch::Interactive(Plan {
        target,
        initial_cwd,
        auto_cwd,
        auto_session: cli.connect.session,
        theme,
        instance_id,
    }))
}
