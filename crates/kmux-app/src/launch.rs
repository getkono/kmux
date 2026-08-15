//! Shared CLI front door for the kmux frontends.
//!
//! The `kmux` entrypoint and the GUI frontends call [`crate::launch::run_cli`]: it
//! initializes logging, parses the CLI, runs any non-interactive subcommand
//! (`ls`, `daemon`, `--dry-run`), or returns a frontend-agnostic [`crate::launch::Plan`]
//! describing the interactive session to launch. Each frontend then builds its
//! own `AppCore` from the plan (supplying its own capabilities) and runs.

use tracing_subscriber::EnvFilter;

use kmux_client::pipeline::ResolvedTarget;

use crate::appearance::Appearance;
use crate::cli::{Cli, Command};
use crate::config::{self, RendererKind};
use crate::subcommands::{
    KickClientConfig, ListClientsConfig, ListSessionsConfig, NotifyConfig, ProcessOverviewConfig,
    parse_target, run_client_command, run_daemon_command, run_debug_command, run_dry_run,
    run_kick_client, run_list_clients, run_list_sessions, run_notify, run_process_overview,
    run_status,
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
        kmux_sys::dirs::client_log_path()
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
    // Cap the `tracing-log` bridge at INFO. kmux logs through `tracing` directly,
    // so this never touches our own events — but uniffi 0.28's `#[uniffi::export]`
    // scaffolding emits a `log::debug!(<fn_name>)` on *every* FFI call (demoted to
    // `trace!` upstream in 0.29). The SwiftUI pump calls ~20 driver getters per
    // frame, so at `RUST_LOG=kmux=debug` those bridged records (`DEBUG kmux_ffi:
    // tabs`, `mode`, …) bury everything else. Gating the `log` facade at the source
    // drops them without an EnvFilter directive that would also silence the
    // renderer's own `tracing::debug!` diagnostics (they share the `kmux_ffi` target).
    log::set_max_level(log::LevelFilter::Info);
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
        protocol_version = %kmux_protocol::messages::PROTOCOL_RANGE,
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

    // Parse with the version string overridden to the full matrix at runtime
    // (clap's derive `version` is a compile-time literal and can't embed the
    // const protocol/render numbers). `-V`/`--version` print
    // `kmux <VersionInfo::long_string()>`; the GUIs show the same matrix in About.
    let cli = {
        use clap::FromArgMatches;
        // clap's `version` wants a `'static` string; the matrix is built at
        // runtime, so leak it once (process-lifetime, negligible).
        let version: &'static str = Box::leak(
            crate::version::VersionInfo::current()
                .long_string()
                .into_boxed_str(),
        );
        let mut cmd = Cli::command().version(version);
        let matches = cmd.get_matches_mut();
        match Cli::from_arg_matches(&matches) {
            Ok(cli) => cli,
            Err(e) => e.format(&mut cmd).exit(),
        }
    };

    // Most subcommands are non-interactive and short-circuit before any frontend
    // setup. `kmux diagnostic <test>` and `kmux open` are the exceptions: they
    // fall through to an interactive launch. `open` carries its own connection
    // args (the explicit form of the bare positional); capture whichever applies
    // into `connect` so the shared dry-run/interactive code below reads one source.
    let mut initial_program: Option<(String, Vec<String>)> = None;
    let mut connect = cli.connect;
    match cli.command {
        Some(Command::Open {
            connect: open_connect,
        }) => {
            connect = open_connect;
        }
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
        Some(Command::Notify {
            kind,
            title,
            body,
            pane,
            server_args,
        }) => {
            // Best-effort: `kmux notify` is a fire-and-forget hook (Claude Code
            // Stop/Notification, shell `precmd`, …). A delivery failure — not in a
            // pane, daemon down, version-skewed client/daemon, connection refused —
            // must never surface as a non-zero exit, or every hook firing spams the
            // caller (e.g. Claude Code's "Stop hook failed" warning after each
            // turn). Log the reason to the client log and exit 0, mirroring the
            // metrics sink's "must never take down the client" rule.
            if let Err(e) = run_notify(NotifyConfig {
                server: server_args.server.as_deref(),
                ssh_port: server_args.ssh_port,
                pane,
                kind: kind.map(Into::into),
                title,
                body,
            })
            .await
            {
                tracing::warn!(target: "kmux::notify", "notify not delivered: {e:#}");
            }
            return Ok(Launch::Done);
        }
        Some(Command::Status { format }) => {
            run_status(format).await?;
            return Ok(Launch::Done);
        }
        Some(Command::Client { action }) => {
            run_client_command(action).await?;
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

    if connect.dry_run && connect.test {
        eprintln!("warning: --test implies --dry-run; running in --test mode.");
    }
    if connect.dry_run || connect.test {
        run_dry_run(&connect.server_args, connect.test).await?;
        return Ok(Launch::Done);
    }

    // Interactive: resolve the connection target, cwd, and theme into a plan.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();
    let (target, parsed_server) = parse_target(
        connect.server_args.server.as_deref(),
        connect.server_args.ssh_port,
    );
    let auto_cwd = connect
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
        auto_session: connect.session,
        theme,
        font,
        appearance,
        cursor_blink,
        renderer,
        instance_id,
        initial_program,
    })))
}
