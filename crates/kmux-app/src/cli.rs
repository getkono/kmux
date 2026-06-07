use clap::{Args, Parser, Subcommand, ValueEnum};

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
    #[arg(long, global = true)]
    pub theme: Option<String>,

    /// GUI font as a Pango font-description string (e.g. "JetBrains Mono 12").
    /// GUI only — the TUI uses the host terminal's font. Falls back to the
    /// `font` key in ~/.config/kmux/config.toml, then "monospace 11".
    #[arg(long, global = true)]
    pub font: Option<String>,
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
