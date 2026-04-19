use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "kmux",
    about = "kmux remote terminal client (TUI)",
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
    #[arg(long, global = true)]
    pub theme: Option<String>,
}

/// Server addressing arguments shared by the default connect action and
/// the `list-sessions` subcommand.
#[derive(Args, Debug)]
pub struct ServerArgs {
    /// Remote server: user@host, user@host:/path, user@host:port, alias
    /// (omit to auto-start and connect to the local daemon)
    pub server: Option<String>,

    /// SSH port to use when connecting to a remote target (overrides hosts.toml)
    #[arg(long)]
    pub ssh_port: Option<u16>,

    // ── Hidden legacy/advanced flags ─────────────────────────────────────────
    /// Server host (prefer positional server argument)
    #[arg(long, hide = true)]
    pub host: Option<String>,

    /// Server port (prefer user@host:port or host:port syntax)
    #[arg(long, hide = true)]
    pub port: Option<u16>,

    /// Auth token (reads from runtime token file if not provided)
    #[arg(long, hide = true)]
    pub token: Option<String>,

    /// Skip SSH tunneling; connect directly via QUIC
    #[arg(long, hide = true)]
    pub no_ssh: bool,

    /// Accept self-signed / invalid TLS certificates
    #[arg(long, hide = true)]
    pub accept_invalid_certs: bool,
}

/// Arguments for connecting to a server (the default action).
#[derive(Args, Debug)]
pub struct ConnectArgs {
    #[command(flatten)]
    pub server_args: ServerArgs,

    /// Auto-attach to a named session (by display name or word_id)
    #[arg(short, long)]
    pub session: Option<String>,

    /// Working directory for a new session (used with --session or user@host:/path)
    #[arg(long)]
    pub cwd: Option<String>,

    /// Trace connection setup end-to-end, verify with one ping, print a
    /// report, and exit without launching the TUI.
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

    /// List sessions on a server without launching the TUI
    #[command(alias = "ls")]
    ListSessions {
        #[command(flatten)]
        server_args: ServerArgs,

        /// Output format
        #[arg(long, default_value = "table")]
        format: OutputFormat,
    },
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
