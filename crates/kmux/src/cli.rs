use clap::{Args, Parser, Subcommand, ValueEnum};
use kmux_client::ssh::{ParsedServer, RemoteTarget, SshSession};
use rand::RngCore;

#[derive(Parser, Debug)]
#[command(name = "kmux", about = "kmux remote terminal client (TUI)", version)]
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

/// Arguments for connecting to a server (the default action).
#[derive(Args, Debug)]
pub struct ConnectArgs {
    /// Remote server: user@host, user@host:/path, user@host:port, alias
    /// (omit to auto-start and connect to the local daemon)
    pub server: Option<String>,

    /// Auto-attach to a named session (by display name or word_id)
    #[arg(short, long)]
    pub session: Option<String>,

    /// Working directory for a new session (used with --session or user@host:/path)
    #[arg(long)]
    pub cwd: Option<String>,

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
}

#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    Table,
    Json,
}

/// Resolved connection parameters ready for use.
pub struct ResolvedConnection {
    pub host: String,
    pub port: u16,
    /// TCP port for headless commands (list-sessions). Falls back to `port` if unset.
    pub tcp_port: Option<u16>,
    pub token: String,
    pub accept_invalid_certs: bool,
    pub is_local: bool,
    pub ssh_session: Option<SshSession>,
    pub ssh_target: Option<RemoteTarget>,
    pub parsed_server: Option<ParsedServer>,
}

pub fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
