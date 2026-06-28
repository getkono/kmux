mod announce;
mod app;
mod auth;
mod capability;
mod capture;
mod client_handler;
mod config;
mod connection;
mod conversions;
mod daemon;
mod engine;
#[cfg(feature = "federation")]
mod federation;
mod handoff;
mod impair;
mod log_writer;
mod persist;
mod process_stats;
mod relay;
mod scrollback;
mod startup;
mod tcp_listener;
mod tls;
mod trace;
mod wordlist;

// The server-side VT pipeline (terminal backend, diff engine, scrollback
// mirror, `TermState`) lives in `kmux-vt-core` so the daemon's in-process path
// and the isolated `kmux-vt-worker` subprocess run identical diff code (issue
// #126). Re-exported at the crate root so existing `crate::backend::…` /
// `crate::diff_engine::…` / `crate::term_state::…` paths keep resolving.
pub use kmux_vt_core::{backend, diff_engine, term_state};

use clap::Parser;
use rand::Rng;
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

    /// Address to bind (deprecated; prefer `[[listen]] bind = "..."` in
    /// `kmuxd.toml`). When omitted, the per-listener `bind` from the config
    /// file (or its built-in default of `0.0.0.0`) is used; passing this
    /// flag overrides every QUIC and TCP+TLS listener.
    #[arg(long)]
    bind: Option<String>,

    /// QUIC port to listen on (0 = ephemeral). Overrides every QUIC listener
    /// when set; absent leaves each listener's configured port intact.
    #[arg(long)]
    port: Option<u16>,

    /// Path to a PEM certificate file. Optional: when no cert/key pair is
    /// configured the daemon generates an in-memory self-signed certificate.
    /// Prefer `[tls] cert = "..."` in kmuxd.toml for persistent configuration.
    #[arg(long)]
    cert: Option<String>,

    /// Path to a PEM private key file. Optional: when no cert/key pair is
    /// configured the daemon generates an in-memory self-signed certificate.
    /// Prefer `[tls] key = "..."` in kmuxd.toml for persistent configuration.
    #[arg(long)]
    key: Option<String>,

    /// Run as a background daemon (double-fork, PID file, Unix socket control).
    /// Daemonization happens before the tokio runtime starts, so fork-safety is maintained.
    #[arg(long)]
    daemon: bool,

    /// TCP+TLS port (0 = ephemeral). Overrides every TCP+TLS listener when
    /// set; absent leaves each listener's configured port intact.
    #[arg(long)]
    tcp_port: Option<u16>,

    /// Pull live PTY sessions from a still-running daemon during a graceful
    /// restart. Set automatically by the outgoing daemon when it spawns its
    /// successor; not intended for manual use. On any failure the daemon falls
    /// back to the normal on-disk snapshot restore.
    #[arg(long)]
    handoff: bool,

    /// Pane VT isolation mode. Overrides `[daemon] session_isolation` in
    /// kmuxd.toml when set. `in-process` (default) keeps the emulator in the
    /// daemon; `process` runs each pane's VT pipeline in an isolated
    /// `kmux-vt-worker` subprocess (issue #126).
    #[arg(long, value_enum)]
    session_isolation: Option<config::SessionIsolationMode>,
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
        // A graceful-restart successor (`--handoff`) must NOT take the pid file
        // here: the predecessor still holds its `flock`. It writes the pid file
        // itself once the predecessor exits (see `startup::async_main`).
        let pid_arg = if cli.handoff {
            None
        } else {
            Some(pid_path.as_path())
        };
        daemon::daemonize_process(pid_arg)?;
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
            // `ResilientWriter` (not the stock `Mutex<File>`) so a write that
            // fails on a full disk degrades to "no logs" instead of poisoning
            // the lock and cascading into worker panics that kill the daemon —
            // the root cause of `kmux daemon restart` failing under disk
            // pressure. See `log_writer`.
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env().add_directive("kmuxd=info".parse()?))
                .with_writer(log_writer::ResilientWriter::new(file))
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
    if let Some(cert) = cli.cert {
        cfg_file.tls.cert = Some(cert);
    }
    if let Some(key) = cli.key {
        cfg_file.tls.key = Some(key);
    }
    // The `--session-isolation` flag (issue #126) overrides the `[daemon]` key.
    if let Some(mode) = cli.session_isolation {
        cfg_file.daemon.session_isolation = mode;
    }
    // Apply bind/port overrides only when the user explicitly passed the
    // corresponding flag. Previously these were eager-defaulted on the CLI
    // (`--bind 0.0.0.0`, `--port 0`), so every invocation silently rewrote
    // every listener's bind+port — making `[[listen]] bind = "..."` in
    // kmuxd.toml impossible to honour.
    for l in &mut cfg_file.listen {
        use config::ListenKind;
        if !l.enabled {
            continue;
        }
        match l.kind {
            ListenKind::Quic => {
                if let Some(bind) = &cli.bind {
                    l.bind = bind.clone();
                }
                if let Some(port) = cli.port {
                    l.port = port;
                }
            }
            ListenKind::TcpTls => {
                if let Some(bind) = &cli.bind {
                    l.bind = bind.clone();
                }
                if let Some(port) = cli.tcp_port {
                    l.port = port;
                }
            }
            ListenKind::Unix => {}
        }
    }

    let server_cfg = config::ServerConfig::resolve(cfg_file)?;

    let rt = tokio::runtime::Runtime::new()?;
    let result = rt.block_on(
        startup::async_main(cli.daemon, cli.handoff, server_cfg)
            .instrument(tracing::info_span!("instance", id = %instance_id)),
    );

    // A graceful-restart predecessor deliberately keeps its migrated PTY children
    // alive (they belong to the successor now), so the per-child `waitpid` reaper
    // threads (`spawn_blocking`, see kmux_pty::process::spawn_wait_task) never
    // return. Dropping the runtime *joins* those blocking threads, which would hang
    // the outgoing daemon forever instead of letting it "completely shut-off"
    // (issue #36). Detach them: shut the runtime down in the background and let
    // process exit reap the threads.
    rt.shutdown_background();

    result
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

/// Remove stale daemon artifacts and spawn `kmuxd --daemon`.
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
