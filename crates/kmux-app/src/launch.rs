//! Shared CLI front door for the kmux frontends.
//!
//! The `kmux` entrypoint and the GUI frontends call [`run_cli`]: it
//! initializes logging, parses the CLI, runs any non-interactive subcommand
//! (`ls`, `daemon`, `--dry-run`), or returns a frontend-agnostic [`Plan`]
//! describing the interactive session to launch. Each frontend then builds its
//! own `AppCore` from the plan (supplying its own capabilities) and runs.

use clap::Parser;
use tracing_subscriber::EnvFilter;

use kmux_client::pipeline::ResolvedTarget;

use crate::appearance::Appearance;
use crate::cli::{Cli, Command};
use crate::config::{self, RendererKind};
use crate::subcommands::{
    KickClientConfig, ListClientsConfig, ListSessionsConfig, ProcessOverviewConfig, parse_target,
    run_daemon_command, run_debug_command, run_dry_run, run_kick_client, run_list_clients,
    run_list_sessions, run_process_overview,
};
use crate::theme::Theme;

/// Outcome of [`run_cli`].
pub enum Launch {
    /// A non-interactive subcommand handled everything; the process should exit.
    Done,
    /// Launch an interactive session with these parameters. Boxed so the small
    /// `Done` variant doesn't pad this enum out to `Plan`'s size.
    Interactive(Box<Plan>),
}

/// Frontend-agnostic interactive launch parameters. Each frontend builds its own
/// `AppCore` (with frontend-specific capabilities) from this.
pub struct Plan {
    pub target: ResolvedTarget,
    pub initial_cwd: String,
    pub auto_cwd: Option<String>,
    pub auto_session: Option<String>,
    pub theme: Theme,
    /// Legacy GUI font (Pango font-description string). Retained for the GTK
    /// preferences font entry; the cell metrics are now derived from
    /// [`appearance`](Self::appearance), which already folds this in.
    pub font: String,
    /// Resolved terminal appearance (font family/size/style, OpenType features,
    /// cell adjustments). The GUI frontends derive their cell metrics from this.
    pub appearance: Appearance,
    /// Whether the inner-pane cursor blinks.
    pub cursor_blink: bool,
    /// Terminal renderer backend resolved from `config.toml` (Cairo by default).
    /// The GTK frontend uses this to decide whether to build the GPU path.
    pub renderer: RendererKind,
    pub instance_id: String,
    /// Run this `(program, args)` in a fresh dedicated initial session instead
    /// of opening a shell. Currently populated only by `kmux diagnostic <test>`
    /// (issue #145); `None` for every ordinary launch.
    pub initial_program: Option<(String, Vec<String>)>,
}

/// Initialize tracing to the client log file (falling back to stderr) and log a
/// startup line with the build/protocol versions.
pub fn init_logging(instance_id: &str) {
    // Honor `RUST_LOG` verbatim when it is set (so e.g. `RUST_LOG=kmux=trace`
    // raises the binary's own logs — an `add_directive("kmux=info")` would
    // override that). Fall back to `kmux=info` only when `RUST_LOG` is unset.
    let filter = || match std::env::var(EnvFilter::DEFAULT_ENV) {
        Ok(rust_log) if !rust_log.trim().is_empty() => EnvFilter::new(rust_log),
        _ => EnvFilter::new("kmux=info"),
    };
    // `KMUX_LOG_STDERR=1` forces logs to stderr instead of the client log file,
    // so crash debugging (e.g. `./kmux`, which sets it) shows the live trace in
    // the terminal alongside any panic backtrace.
    let force_stderr = std::env::var_os("KMUX_LOG_STDERR")
        .is_some_and(|v| !v.is_empty() && v != "0" && v != "false");
    let log_file = if force_stderr {
        None
    } else {
        kmux_protocol::dirs::client_log_path()
            .and_then(|p| {
                Ok(std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)?)
            })
            .ok()
    };
    match log_file {
        Some(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => {
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
    use clap::CommandFactory;
    // MUST stay first: clap_complete requires that nothing is written to stdout
    // before this runs. On a completion request (the `COMPLETE=<shell>` env var
    // is set) it prints the completions/registration script and exits the
    // process; otherwise it returns and normal parsing proceeds. Placing it here
    // — the shared CLI front door, ahead of logging and any GUI handoff — gives
    // every frontend identical, always-in-sync dynamic completion for free.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    init_logging(&instance_id);

    let cli = Cli::parse();

    // Most subcommands are non-interactive and short-circuit before any frontend
    // setup. `kmux diagnostic <test>` is the exception: it falls through to an
    // interactive launch, carrying the emitter program to run in the session.
    let mut initial_program: Option<(String, Vec<String>)> = None;
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
        Some(Command::Ps {
            server_args,
            format,
        }) => {
            run_process_overview(ProcessOverviewConfig {
                server: server_args.server.as_deref(),
                ssh_port: server_args.ssh_port,
                format,
            })
            .await?;
            return Ok(Launch::Done);
        }
        Some(Command::Clients {
            session,
            server,
            ssh_port,
            format,
        }) => {
            run_list_clients(ListClientsConfig {
                server: server.as_deref(),
                ssh_port,
                session,
                format,
            })
            .await?;
            return Ok(Launch::Done);
        }
        Some(Command::Kick {
            session,
            client,
            server,
            ssh_port,
        }) => {
            run_kick_client(KickClientConfig {
                server: server.as_deref(),
                ssh_port,
                session,
                client,
            })
            .await?;
            return Ok(Launch::Done);
        }
        Some(Command::Debug { action }) => {
            run_debug_command(action).await?;
            return Ok(Launch::Done);
        }
        Some(Command::Diagnostic { test, emit }) => {
            if emit {
                // Internal: the spawned session runs `kmux diagnostic <test> --emit`.
                crate::diagnostic::emit(test.unwrap_or(crate::diagnostic::DiagnosticTest::All))?;
                return Ok(Launch::Done);
            }
            match test {
                // `kmux diagnostic` with no test lists the patterns and exits.
                None => {
                    crate::diagnostic::print_catalogue();
                    return Ok(Launch::Done);
                }
                // `kmux diagnostic <test>` opens the GUI with a session running
                // the emitter; resolve it now so a missing `kmux` binary fails
                // here (before any GUI handoff) with a clear message.
                Some(test) => {
                    initial_program = Some(crate::diagnostic::session_command(test)?);
                }
            }
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
    let font = config::resolve_font(cli.font.as_deref());
    let appearance = config::resolve_appearance(cli.font.as_deref());
    let cursor_blink = config::resolve_cursor_blink(cli.cursor_blink);
    let renderer = config::resolve_renderer();

    Ok(Launch::Interactive(Box::new(Plan {
        target,
        initial_cwd,
        auto_cwd,
        auto_session: cli.connect.session,
        theme,
        font,
        appearance,
        cursor_blink,
        renderer,
        instance_id,
        initial_program,
    })))
}
