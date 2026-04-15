use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::app::ServerApp;

/// Daemonize the current process (double-fork) and write the child PID to `pid_path`.
///
/// This MUST be called before any tokio runtime is created — fork and async
/// runtimes do not mix safely.
pub fn daemonize_process(pid_path: &Path) -> anyhow::Result<()> {
    use daemonize::Daemonize;

    Daemonize::new()
        .pid_file(pid_path)
        .working_directory("/")
        .umask(0o077)
        .start()
        .map_err(|e| anyhow::anyhow!("daemonize failed: {e}"))?;

    Ok(())
}

/// JSON request from a control socket client.
#[derive(Deserialize)]
struct ControlRequest {
    command: String,
}

/// JSON response sent back to a control socket client.
#[derive(Serialize)]
struct StatusResponse {
    status: &'static str,
    port: u16,
    token: String,
    pid: u32,
    uptime_secs: u64,
    session_count: usize,
}

/// JSON response for the stop command.
#[derive(Serialize)]
struct StopResponse {
    status: &'static str,
}

/// Bind the Unix control socket and serve status queries.
///
/// Removes any stale socket file before binding. Each accepted connection
/// receives exactly one request/response exchange then closes.
///
/// Run this as a `tokio::spawn`ed background task after the QUIC endpoint is
/// bound and the actual port is known.
pub async fn serve_control_socket(
    socket_path: PathBuf,
    pid_path: PathBuf,
    port: u16,
    token: String,
    start_time: Instant,
    app: Arc<ServerApp>,
    shutdown: Arc<Notify>,
) {
    // Remove stale socket if it exists from a previous run.
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!(
                "Failed to bind control socket {}: {e}",
                socket_path.display()
            );
            return;
        }
    };
    info!("Control socket listening on {}", socket_path.display());

    // Register a cleanup guard so the socket file is removed when this task exits.
    let socket_cleanup = socket_path.clone();
    let pid_cleanup = pid_path.clone();
    let _guard = SocketGuard {
        socket_path: socket_cleanup,
        pid_path: pid_cleanup,
    };

    // Install signal handlers for graceful shutdown.
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to install SIGTERM handler: {e}");
            return;
        }
    };

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let token = token.clone();
                        let app = Arc::clone(&app);
                        let shutdown = Arc::clone(&shutdown);
                        tokio::spawn(async move {
                            handle_control_connection(stream, port, &token, start_time, app, shutdown).await;
                        });
                    }
                    Err(e) => {
                        warn!("Control socket accept error: {e}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down daemon");
                shutdown.notify_waiters();
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down daemon");
                shutdown.notify_waiters();
                break;
            }
        }
    }
    // _guard drops here, cleaning up socket and pid files.
}

async fn handle_control_connection(
    stream: tokio::net::UnixStream,
    port: u16,
    token: &str,
    start_time: Instant,
    app: Arc<ServerApp>,
    shutdown: Arc<Notify>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    if let Err(e) = reader.read_line(&mut line).await {
        warn!("Control socket read error: {e}");
        return;
    }

    let req: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            warn!("Invalid control request: {e}");
            return;
        }
    };

    match req.command.as_str() {
        "status" => {
            let session_count = app.list_sessions().await.len();
            let response = StatusResponse {
                status: "running",
                port,
                token: token.to_string(),
                pid: std::process::id(),
                uptime_secs: start_time.elapsed().as_secs(),
                session_count,
            };
            let mut json = match serde_json::to_string(&response) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize status response: {e}");
                    return;
                }
            };
            json.push('\n');
            if let Err(e) = write_half.write_all(json.as_bytes()).await {
                warn!("Control socket write error: {e}");
            }
        }
        "stop" => {
            let response = StopResponse { status: "ok" };
            let mut json = match serde_json::to_string(&response) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize stop response: {e}");
                    return;
                }
            };
            json.push('\n');
            if let Err(e) = write_half.write_all(json.as_bytes()).await {
                warn!("Control socket write error: {e}");
            }
            info!("Received stop command, shutting down daemon");
            shutdown.notify_waiters();
        }
        other => {
            warn!("Unknown control command: {other}");
        }
    }
}

/// RAII guard that removes the socket and PID files when dropped.
struct SocketGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.pid_path);
    }
}
