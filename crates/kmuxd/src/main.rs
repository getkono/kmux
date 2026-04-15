mod app;
mod auth;
mod backend;
mod capability;
mod connection;
mod daemon;
mod diff_engine;
mod persist;
mod relay;
mod scrollback;
mod tcp_listener;
mod term_state;
mod tls;
mod wordlist;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use clap::Parser;
use rand::RngCore;
use tracing::{Instrument, error, info, warn};
use tracing_subscriber::EnvFilter;

use app::ServerApp;
use auth::{generate_token, persist_token};

#[derive(Parser, Debug)]
#[command(name = "kmuxd", about = "kmux remote terminal server")]
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
    tracing::info!(instance_id = %instance_id, "kmuxd started");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cli).instrument(tracing::info_span!("instance", id = %instance_id)))?;

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

async fn async_main(cli: Cli) -> anyhow::Result<()> {
    info!(backend = term_state::BACKEND_NAME, "terminal backend");

    let tls_config = if cli.self_signed {
        let (cert, key) = tls::generate_self_signed()?;
        tls::build_tls_config(cert, key)?
    } else {
        let cert_path = cli
            .cert
            .ok_or_else(|| anyhow::anyhow!("--cert is required without --self-signed"))?;
        let key_path = cli
            .key
            .ok_or_else(|| anyhow::anyhow!("--key is required without --self-signed"))?;
        tls::load_tls_config(&cert_path, &key_path)?
    };

    let quinn_config = tls::build_quinn_config(tls_config)?;

    let token = generate_token();
    match persist_token(&token) {
        Ok(path) => info!("Auth token persisted to {}", path.display()),
        Err(e) => tracing::warn!("Failed to persist auth token: {e}"),
    }
    println!("Auth token: {token}");

    let app = Arc::new(ServerApp::new(token.clone()));

    // Restore persisted sessions from the previous daemon instance, if any.
    if let Ok(path) = kmux_protocol::dirs::session_state_path()
        && path.exists()
    {
        match persist::restore::read_checkpoint(&path) {
            Ok(state) => {
                let report = app.restore_from(state).await;
                info!(
                    restored = report.restored,
                    alive = report.alive,
                    dead = report.dead,
                    "session restore complete"
                );
            }
            Err(e) => warn!("failed to restore sessions from checkpoint: {e}"),
        }
    }

    // Periodic checkpoint task: saves session state every 30 seconds for
    // crash recovery. Does NOT set keep_alive (children may still be killed
    // by the kernel if the daemon crashes).
    {
        let persist_app = Arc::clone(&app);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let state = persist_app.checkpoint_state().await;
                match kmux_protocol::dirs::session_state_path() {
                    Ok(path) => {
                        if let Err(e) = persist::checkpoint::write_checkpoint(&state, &path) {
                            warn!("periodic checkpoint failed: {e}");
                        }
                    }
                    Err(e) => warn!("could not determine checkpoint path: {e}"),
                }
            }
        });
    }

    let addr: SocketAddr = format!("{}:{}", cli.bind, cli.port).parse()?;
    let endpoint = quinn::Endpoint::server(quinn_config, addr)?;
    let actual_addr = endpoint.local_addr()?;
    let actual_port = actual_addr.port();
    info!("Listening on quic://{actual_addr}");

    // Start the TCP fallback/tunnel transport listener.
    let tcp_bind: SocketAddr = format!("{}:{}", cli.bind, cli.tcp_port).parse()?;
    let tcp_port = tcp_listener::serve_tcp(tcp_bind, Arc::clone(&app)).await?;

    let shutdown = Arc::new(Notify::new());

    if cli.daemon {
        let socket_path = kmux_protocol::dirs::socket_path()?;
        let pid_path = kmux_protocol::dirs::pid_path()?;
        let start_time = Instant::now();
        let token_clone = token.clone();
        let app_clone = Arc::clone(&app);
        let shutdown_clone = Arc::clone(&shutdown);
        tokio::spawn(async move {
            daemon::serve_control_socket(
                socket_path,
                pid_path,
                actual_port,
                tcp_port,
                token_clone,
                start_time,
                app_clone,
                shutdown_clone,
            )
            .await;
        });
    }

    // Install signal handlers for the foreground (non-daemon) case.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                match incoming {
                    Some(incoming) => {
                        let app = Arc::clone(&app);
                        tokio::spawn(async move {
                            match incoming.await {
                                Ok(conn) => {
                                    let remote = conn.remote_address();
                                    info!("QUIC connection from {remote}");
                                    connection::handle(conn, app).await;
                                }
                                Err(e) => error!("QUIC connection failed: {e}"),
                            }
                        });
                    }
                    None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down");
                break;
            }
            _ = shutdown.notified() => {
                info!("Shutdown requested via control socket");
                break;
            }
        }
    }

    // Clean shutdown: checkpoint the full session state so the next daemon
    // start can replay the visual content as preamble in fresh shells.
    let shutdown_state = app.checkpoint_state().await;
    match kmux_protocol::dirs::session_state_path() {
        Ok(path) => {
            if let Err(e) = persist::checkpoint::write_checkpoint(&shutdown_state, &path) {
                warn!("shutdown checkpoint failed: {e}");
            } else {
                info!("session state checkpointed on shutdown");
            }
        }
        Err(e) => warn!("could not determine checkpoint path on shutdown: {e}"),
    }

    endpoint.close(0u32.into(), b"shutdown");
    Ok(())
}

fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
