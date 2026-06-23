use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCandidates;

#[derive(Parser, Debug)]
#[command(
    name = "kmux",
    about = "kmux remote terminal client",
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
    )
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub connect: ConnectArgs,

    /// Color theme: built-in name (one-dark, catppuccin-latte, catppuccin-frappe,
    /// catppuccin-macchiato, catppuccin-mocha, dracula) or a custom theme name
    /// from ~/.config/kmux/themes/<name>.toml
    #[arg(long, global = true, add = ArgValueCandidates::new(crate::completion::theme_candidates))]
    pub theme: Option<String>,

    /// GUI font as a Pango font-description string (e.g. "JetBrains Mono 12").
    /// Falls back to the `font` key in ~/.config/kmux/config.toml, then
    /// "monospace 11". Deprecated in favor of the structured `font-family` /
    /// `font-size` config keys (see docs/appearance.md); still honored as the
    /// family/size fallback when those are unset.
    #[arg(long, global = true)]
    pub font: Option<String>,

    /// Whether the inner-pane cursor blinks (`--cursor-blink true|false`).
    /// Falls back to the `cursor_blink` key in ~/.config/kmux/config.toml,
    /// then `true`.
    #[arg(long, global = true)]
    pub cursor_blink: Option<bool>,
}

/// Server addressing arguments shared by the default connect action and
/// the `list-sessions` subcommand.
///
/// Every port in the server string is the **SSH** port. Daemon data-plane
/// ports (QUIC, TCP+TLS) are ephemeral and exchanged in-band via the
/// authenticated SSH handshake — they never appear on the command line.
#[derive(Args, Debug)]
pub struct ServerArgs {
    /// Remote SSH target: `[user@]host[:ssh-port][:/path]` or a `hosts.toml`
    /// alias. Omit to connect to the local daemon.
    #[arg(add = ArgValueCandidates::new(crate::completion::server_candidates))]
    pub server: Option<String>,

    /// Override the SSH port for the target (also settable via `host:port`
    /// in the server string or `ssh_port` in `hosts.toml`).
    #[arg(long)]
    pub ssh_port: Option<u16>,
}

/// Arguments for connecting to a server (the default action).
#[derive(Args, Debug)]
pub struct ConnectArgs {
    #[command(flatten)]
    pub server_args: ServerArgs,

    /// Auto-attach to a named session (by display name or word_id)
    #[arg(short, long, add = ArgValueCandidates::new(crate::completion::session_candidates))]
    pub session: Option<String>,

    /// Working directory for a new session (used with --session or user@host:/path)
    #[arg(long)]
    pub cwd: Option<String>,

    /// Trace connection setup end-to-end, verify with one ping, print a
    /// report, and exit without launching the GUI.
    #[arg(long, short = 'n')]
    pub dry_run: bool,

    /// Superset of `--dry-run`: also run the `TransportSupervisor` live for
    /// ~10 seconds so transport scoring and any hot-swap upgrade are
    /// visible. Implies `--dry-run`.
    #[arg(long)]
    pub test: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage the local kmux daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// List sessions on a server without launching the GUI
    #[command(alias = "ls")]
    ListSessions {
        #[command(flatten)]
        server_args: ServerArgs,

        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },

    /// Show the process tree of every pane (CPU/memory) without the GUI (issue
    /// #122). The hierarchical counterpart of `ls`.
    #[command(alias = "top")]
    Ps {
        #[command(flatten)]
        server_args: ServerArgs,

        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },

    /// List the client connections attached to sessions on a server (issue #146).
    /// Each connection shows its user-readable label, machine id, hostname, and
    /// which session/panes it is viewing. Pass a session word-id to scope to one.
    ///
    /// The session is the leading positional; address a remote daemon with
    /// `--server` (mirrors `ls`/`ps`, which take the server positionally because
    /// it is their only argument).
    Clients {
        /// Limit to one session (word-id); omit to list every session's clients.
        #[arg(value_name = "SESSION")]
        session: Option<String>,

        /// Target server: `[user@]host[:ssh-port][:/path]` or a `hosts.toml`
        /// alias. Omit to use the local daemon.
        #[arg(long, add = ArgValueCandidates::new(crate::completion::server_candidates))]
        server: Option<String>,

        /// Override the SSH port for the target server.
        #[arg(long)]
        ssh_port: Option<u16>,

        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },

    /// Kick one client connection out of a session (issue #146): the target is
    /// detached from the session's panes (its connection stays alive). Identify
    /// the client by its user-readable label or numeric client-id from `clients`.
    ///
    /// `session` and `client` are required positionals; address a remote daemon
    /// with `--server`.
    Kick {
        /// Session word-id the client is attached to.
        #[arg(value_name = "SESSION")]
        session: String,

        /// The client to kick: its label (e.g. `alice@box`) or numeric client-id.
        #[arg(value_name = "CLIENT")]
        client: String,

        /// Target server: `[user@]host[:ssh-port][:/path]` or a `hosts.toml`
        /// alias. Omit to use the local daemon.
        #[arg(long, add = ArgValueCandidates::new(crate::completion::server_candidates))]
        server: Option<String>,

        /// Override the SSH port for the target server.
        #[arg(long)]
        ssh_port: Option<u16>,
    },

    /// Internal diagnostics (hidden). See `kmux debug tearing` (issue #72).
    #[command(hide = true)]
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },

    /// Open a session running a render diagnostic test pattern, to visually
    /// verify glyph/color rendering (issue #145). Run without a test to list
    /// the available patterns.
    Diagnostic {
        /// Which test pattern to run; omit to list the available patterns.
        #[arg(value_name = "TEST")]
        test: Option<crate::diagnostic::DiagnosticTest>,

        /// Emit the pattern to stdout and hold the pane open (internal: the
        /// launched session runs this; also usable to test the host terminal).
        #[arg(long, hide = true)]
        emit: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DebugAction {
    /// Analyze daemon + client frame traces (captured with `KMUX_FRAME_TRACE=1`)
    /// and report logical frames that were painted partially (tearing).
    Tearing {
        /// Daemon trace JSONL. Defaults to `<state_dir>/frame_trace_daemon.jsonl`.
        #[arg(long)]
        daemon_trace: Option<std::path::PathBuf>,
        /// Client trace JSONL. Defaults to `<state_dir>/frame_trace_client.jsonl`.
        #[arg(long)]
        client_trace: Option<std::path::PathBuf>,
        /// Logical-frame coalescing window in milliseconds: daemon diffs whose
        /// send-time gaps are below this are treated as one logical frame.
        #[arg(long, default_value_t = 16)]
        window_ms: u64,
    },

    /// Print the resolved profile-specific paths — client/daemon logs, runtime
    /// and state dirs — plus the `kmuxd` binary an auto-spawn would launch.
    ///
    /// Debug builds isolate these under `kmux-debug/`; release builds use
    /// `kmux/`. Use this to find where a `cargo run` / `swift run` GUI is
    /// actually logging, and which daemon it would start.
    Paths,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
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
    /// List sessions and their active connections
    Sessions {
        /// Show all sessions, including those with no active connections
        #[arg(short, long)]
        all: bool,
        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

/// Resolved connection parameters for headless subcommands (e.g. list-sessions).
pub struct ResolvedConnection {
    pub host: String,
    pub port: u16,
    /// TCP port for headless commands. Falls back to `port` if unset.
    pub tcp_port: Option<u16>,
    pub token: String,
}
