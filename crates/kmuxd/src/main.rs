mod announce;
mod app;
mod auth;
mod backend;
mod capability;
mod client_handler;
mod config;
mod connection;
mod conversions;
mod daemon;
mod diff_engine;
mod persist;
mod relay;
mod scrollback;
mod startup;
mod tcp_listener;
mod term_state;
mod tls;
mod wordlist;

use clap::Parser;
use rand::RngCore;
use tracing::Instrument;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "kmuxd",
    about = "kmux remote terminal server",
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
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to a `kmuxd.toml` config file. When absent, config is discovered at
    /// `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml` or `/etc/kmuxd/kmuxd.toml`.
    #[arg(long)]
    config: Option<std::path::PathBuf>,

    /// Address to bind (default: all interfaces).
    /// Deprecated: prefer `[[listen]] bind = "..."` in kmuxd.toml.
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Port to listen on (0 = pick a random available port).
    /// Deprecated: prefer `[[listen]] port = N` in kmuxd.toml.
    #[arg(long, default_value_t = 8443)]
    port: u16,

    /// Path to a PEM certificate file (required unless --self-signed).
    /// Deprecated: prefer `[tls] cert = "..."` in kmuxd.toml.
    #[arg(long)]
    cert: Option<String>,

    /// Path to a PEM private key file (required unless --self-signed).
    /// Deprecated: prefer `[tls] key = "..."` in kmuxd.toml.
    #[arg(long)]
    key: Option<String>,

    /// Generate an in-memory self-signed certificate (for development).
    /// Deprecated: prefer `[tls] self_signed = true` in kmuxd.toml.
    #[arg(long)]
    self_signed: bool,

    /// Run as a background daemon (double-fork, PID file, Unix socket control).
    /// Daemonization happens before the tokio runtime starts, so fork-safety is maintained.
    #[arg(long)]
    daemon: bool,

    /// TCP port for the fallback/tunnel transport (0 = pick a random available port).
    /// Deprecated: prefer `[[listen]] kind = "tcp+tls"` in kmuxd.toml.
    #[arg(long, default_value_t = 0)]
    tcp_port: u16,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Probe for a running daemon or start one, then print connection JSON to stdout.
    ///
    /// Output format (JSON):
    ///   `{"protocol_version": N, "kmuxd_version": "...", "quic_port": N, "tcp_port": N,
    ///     "token": "...", "endpoints": [...]}`
    ///
    /// Designed for SSH-based auto-negotiation: `ssh user@host kmuxd probe-or-start`.
    /// Exits 0 on success; exits 1 with an error message on stderr on failure.
    ProbeOrStart,

    /// Print the effective configuration (defaults merged with the config file) and exit.
    PrintConfig {
        /// Path to the config file (overrides standard search order).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    // Parse CLI before daemonizing so --help/--version work in the foreground.
    let cli = Cli::parse();

    // probe-or-start: short-lived query/start, no need to daemonize or init full logging.
    if let Some(Command::ProbeOrStart) = cli.command {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(probe_or_start());
    }

    // print-config: dump effective config and exit.
    if let Some(Command::PrintConfig { config: cfg_path }) = &cli.command {
        let (cfg, source) = config::load_config(cfg_path.as_deref())?;
        match source {
            Some(p) => eprintln!("# Loaded from: {}", p.display()),
            None => eprintln!("# Using built-in defaults (no config file found)"),
        }
        println!("{}", toml::to_string_pretty(&cfg)?);
        return Ok(());
    }

    if cli.daemon {
        let pid_path = kmux_protocol::dirs::pid_path()?;
        daemon::daemonize_process(&pid_path)?;
        // After this point we are in the daemonized child process with fresh fds.
    }

    // Initialize tracing after daemonize (child process has fresh fds).
    // Log to a persistent file; fall back to stderr if the path can't be opened.
    let instance_id = generate_instance_id();
    match kmux_protocol::dirs::daemon_log_path().and_then(|p| {
        Ok(std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)?)
    }) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env().add_directive("kmuxd=info".parse()?))
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env().add_directive("kmuxd=info".parse()?))
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
        "kmuxd started"
    );

    // Load config and apply deprecated CLI overrides.
    let (mut cfg_file, cfg_source) = config::load_config(cli.config.as_deref())?;
    if cfg_source.is_none() {
        // No config file found: write a default template on first run.
        if let Ok(xdg_cfg) = std::env::var("XDG_CONFIG_HOME") {
            let default_path = std::path::PathBuf::from(xdg_cfg)
                .join("kmuxd")
                .join("kmuxd.toml");
            if let Err(e) = config::write_default_config(&default_path) {
                tracing::warn!(
                    "Could not write default config to {}: {e}",
                    default_path.display()
                );
            } else {
                tracing::info!("Wrote default config to {}", default_path.display());
            }
        }
    }

    // Apply deprecated CLI overrides (they win over the config file when present).
    if cli.self_signed {
        cfg_file.tls.self_signed = true;
    }
    if let Some(cert) = cli.cert {
        cfg_file.tls.cert = Some(cert);
    }
    if let Some(key) = cli.key {
        cfg_file.tls.key = Some(key);
    }
    // Apply bind/port overrides to all matching listeners (deprecated flags).
    for l in &mut cfg_file.listen {
        use config::ListenKind;
        match l.kind {
            ListenKind::Quic if l.enabled => {
                l.bind = cli.bind.clone();
                l.port = cli.port;
            }
            ListenKind::TcpTls if l.enabled => {
                l.bind = cli.bind.clone();
                if cli.tcp_port != 0 {
                    l.port = cli.tcp_port;
                }
            }
            _ => {}
        }
    }

    let server_cfg = config::ServerConfig::resolve(cfg_file)?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(
        startup::async_main(cli.daemon, server_cfg)
            .instrument(tracing::info_span!("instance", id = %instance_id)),
    )?;

    Ok(())
}

/// Implementation of the `probe-or-start` subcommand.
///
/// Queries the local daemon control socket. If the daemon is not running,
/// starts it and polls until it responds.  Prints extended connection JSON to stdout.
///
/// Output includes `protocol_version` and `kmuxd_version` so clients can detect
/// mismatches before attempting a full connection.
async fn probe_or_start() -> anyhow::Result<()> {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// Query the Unix control socket and return the full status response on success.
    async fn query() -> Option<serde_json::Value> {
        let socket_path = kmux_protocol::dirs::socket_path().ok()?;
        let stream =
            tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
                .await
                .ok()?
                .ok()?;

        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(b"{\"command\":\"status\"}\n")
            .await
            .ok()?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .ok()?
            .ok()?;

        #[derive(serde::Deserialize)]
        struct Resp {
            port: u16,
            #[serde(default)]
            tcp_port: u16,
            token: String,
            pid: u32,
            /// Daemon-provided endpoint list (Phase 7+); absent in older builds.
            #[serde(default)]
            endpoints: Vec<serde_json::Value>,
        }
        let resp: Resp = serde_json::from_str(line.trim()).ok()?;

        // Verify the reported PID is alive.
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        if kill(Pid::from_raw(resp.pid as i32), None).is_err() {
            return None;
        }

        // Build the SSH-filtered endpoint list.
        // When the daemon includes `endpoints`, filter them for SSH callers (audience = Any | SshOnly).
        // Older daemons omit `endpoints`; fall back to deriving from raw ports.
        let endpoints: serde_json::Value = if resp.endpoints.is_empty() {
            // Fallback: load config and build the list via announce.rs with SSH path.
            let ssh_endpoints = build_ssh_endpoints(resp.port, resp.tcp_port);
            serde_json::json!(ssh_endpoints)
        } else {
            // Daemon-provided list is already Uds-path-filtered; re-filter for SSH callers.
            // For now, forward the full list — the audience is not serialized in the response.
            serde_json::json!(resp.endpoints)
        };

        // Build the extended probe-or-start JSON (backward-compatible: adds new fields).
        let json = serde_json::json!({
            "protocol_version": kmux_protocol::messages::PROTOCOL_VERSION,
            "kmuxd_version": env!("CARGO_PKG_VERSION"),
            "quic_port": resp.port,
            "tcp_port": resp.tcp_port,
            "token": resp.token,
            "endpoints": endpoints,
        });
        Some(json)
    }

    // Fast path — already running.
    if let Some(json) = query().await {
        println!("{json}");
        return Ok(());
    }

    // Slow path — start a new daemon instance.
    cleanup_and_start_daemon()?;

    // Poll until ready (up to 10 s — remote machines can be slower to start).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        if let Some(json) = query().await {
            println!("{json}");
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    anyhow::bail!("timed out waiting for local kmuxd to start")
}

/// Build a fallback endpoint list for SSH callers from raw port numbers.
///
/// Used when the running daemon does not yet support the `endpoints` field in
/// its control-socket status response. Applies `BootstrapPath::Ssh` audience
/// filtering so only `Any` and `SshOnly` listeners are included.
fn build_ssh_endpoints(quic_port: u16, tcp_port: u16) -> Vec<serde_json::Value> {
    use announce::{BootstrapPath, build_endpoint_list};
    use config::{Audience, ListenConfig, ListenKind};

    // Synthetic listener configs from the running ports; no config file needed.
    let mut listeners = Vec::new();
    if quic_port != 0 {
        listeners.push(ListenConfig {
            kind: ListenKind::Quic,
            bind: "127.0.0.1".to_string(),
            port: quic_port,
            enabled: true,
            path: String::new(),
            audience: Audience::Any,
            priority: 0,
        });
    }
    if tcp_port != 0 {
        listeners.push(ListenConfig {
            kind: ListenKind::TcpTls,
            bind: "127.0.0.1".to_string(),
            port: tcp_port,
            enabled: true,
            path: String::new(),
            audience: Audience::SshOnly,
            priority: 0,
        });
    }

    build_endpoint_list(&listeners, BootstrapPath::Ssh, None)
        .into_iter()
        .map(|a| serde_json::json!({"kind": format!("{}", a.kind), "address": a.address}))
        .collect()
}

/// Remove stale daemon artifacts and spawn `kmuxd --daemon --self-signed`.
fn cleanup_and_start_daemon() -> anyhow::Result<()> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let pid_path = kmux_protocol::dirs::pid_path()?;
    let socket_path = kmux_protocol::dirs::socket_path()?;

    // If a stale PID file exists, kill the old process.
    if pid_path.exists()
        && let Ok(contents) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = contents.trim().parse::<u32>()
    {
        let nix_pid = Pid::from_raw(pid as i32);
        if kill(nix_pid, None).is_ok() {
            let _ = kill(nix_pid, Signal::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(300));
            if kill(nix_pid, None).is_ok() {
                let _ = kill(nix_pid, Signal::SIGKILL);
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    // Resolve the path of the current executable (i.e., this very binary).
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("kmuxd"));

    std::process::Command::new(&exe)
        .args(kmux_protocol::control_rpc::DAEMON_BOOT_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn {}: {e}", exe.display()))?;

    Ok(())
}

fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
