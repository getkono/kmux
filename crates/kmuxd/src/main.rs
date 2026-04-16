mod app;
mod auth;
mod backend;
mod capability;
mod client_handler;
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
#[command(name = "kmuxd", about = "kmux remote terminal server", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Address to bind (default: all interfaces)
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Port to listen on (0 = pick a random available port)
    #[arg(long, default_value_t = 8443)]
    port: u16,

    /// Path to a PEM certificate file (required unless --self-signed)
    #[arg(long)]
    cert: Option<String>,

    /// Path to a PEM private key file (required unless --self-signed)
    #[arg(long)]
    key: Option<String>,

    /// Generate an in-memory self-signed certificate (for development)
    #[arg(long)]
    self_signed: bool,

    /// Run as a background daemon (double-fork, PID file, Unix socket control).
    /// Daemonization happens before the tokio runtime starts, so fork-safety is maintained.
    #[arg(long)]
    daemon: bool,

    /// TCP port for the fallback/tunnel transport (0 = pick a random available port).
    /// Always enabled; defaults to 0 (random) in daemon mode.
    #[arg(long, default_value_t = 0)]
    tcp_port: u16,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Probe for a running daemon or start one, then print connection JSON to stdout.
    ///
    /// Output format: {"quic_port": N, "tcp_port": N, "token": "..."}
    ///
    /// Designed for SSH-based auto-negotiation: `ssh user@host kmuxd probe-or-start`.
    /// Exits 0 on success; exits 1 with an error message on stderr on failure.
    ProbeOrStart,
}

fn main() -> anyhow::Result<()> {
    // Parse CLI before daemonizing so --help/--version work in the foreground.
    let cli = Cli::parse();

    // probe-or-start: short-lived query/start, no need to daemonize or init full logging.
    if let Some(Command::ProbeOrStart) = cli.command {
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(probe_or_start());
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
        version = env!("CARGO_PKG_VERSION"),
        protocol_version = kmux_protocol::messages::PROTOCOL_VERSION,
        "kmuxd started"
    );

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(
        startup::async_main(cli).instrument(tracing::info_span!("instance", id = %instance_id)),
    )?;

    Ok(())
}

/// Implementation of the `probe-or-start` subcommand.
///
/// Queries the local daemon control socket. If the daemon is not running,
/// starts it and polls until it responds.  Prints connection JSON to stdout.
async fn probe_or_start() -> anyhow::Result<()> {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// Query the Unix control socket and return (quic_port, tcp_port, token) on success.
    async fn query() -> Option<(u16, u16, String)> {
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
        }
        let resp: Resp = serde_json::from_str(line.trim()).ok()?;

        // Verify the reported PID is alive.
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        if kill(Pid::from_raw(resp.pid as i32), None).is_err() {
            return None;
        }

        Some((resp.port, resp.tcp_port, resp.token))
    }

    // Fast path — already running.
    if let Some((quic_port, tcp_port, token)) = query().await {
        let json = serde_json::json!({
            "quic_port": quic_port,
            "tcp_port": tcp_port,
            "token": token,
        });
        println!("{json}");
        return Ok(());
    }

    // Slow path — start a new daemon instance.
    cleanup_and_start_daemon()?;

    // Poll until ready (up to 10 s — remote machines can be slower to start).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;

        if let Some((quic_port, tcp_port, token)) = query().await {
            let json = serde_json::json!({
                "quic_port": quic_port,
                "tcp_port": tcp_port,
                "token": token,
            });
            println!("{json}");
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }

    anyhow::bail!("timed out waiting for local kmuxd to start")
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
        .args([
            "--daemon",
            "--self-signed",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
        ])
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
