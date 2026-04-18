use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::announce::{BootstrapPath, build_endpoint_list};
use crate::app::ServerApp;
use crate::config::ListenConfig;
use kmux_protocol::control_rpc::{ControlRequest, EndpointEntry, StatusResponse, StopResponse};

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

/// Shared state used to respond to each control socket request.
#[derive(Clone)]
struct RequestCtx {
    quic_port: u16,
    tcp_port: u16,
    token: String,
    start_time: Instant,
    app: Arc<ServerApp>,
    shutdown: Arc<Notify>,
    listeners: Vec<ListenConfig>,
    public_host: Option<String>,
}

/// Parameters for the Unix control socket server.
pub struct ControlSocketParams {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    /// Actual bound QUIC port (0 = not listening).
    pub quic_port: u16,
    /// Actual bound TCP+TLS port (0 = not listening).
    pub tcp_port: u16,
    pub token: String,
    pub start_time: Instant,
    pub app: Arc<ServerApp>,
    pub shutdown: Arc<Notify>,
    /// Resolved listener configs (with actual bound ports filled in).
    pub listeners: Vec<ListenConfig>,
    pub public_host: Option<String>,
}

/// Bind the Unix control socket and serve status queries.
///
/// Removes any stale socket file before binding. Each accepted connection
/// receives exactly one request/response exchange then closes.
///
/// Run this as a `tokio::spawn`ed background task after the QUIC endpoint is
/// bound and the actual port is known.
pub async fn serve_control_socket(params: ControlSocketParams) {
    let ControlSocketParams {
        socket_path,
        pid_path,
        quic_port,
        tcp_port,
        token,
        start_time,
        app,
        shutdown,
        listeners,
        public_host,
    } = params;

    let ctx = RequestCtx {
        quic_port,
        tcp_port,
        token,
        start_time,
        app,
        shutdown: Arc::clone(&shutdown),
        listeners,
        public_host,
    };
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
                        let ctx = ctx.clone();
                        tokio::spawn(async move {
                            handle_control_connection(stream, ctx).await;
                        });
                    }
                    Err(e) => {
                        warn!("Control socket accept error: {e}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT, shutting down daemon");
                ctx.shutdown.notify_waiters();
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM, shutting down daemon");
                ctx.shutdown.notify_waiters();
                break;
            }
        }
    }
    // _guard drops here, cleaning up socket and pid files.
}

async fn handle_control_connection(stream: tokio::net::UnixStream, ctx: RequestCtx) {
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
            let session_count = ctx.app.list_sessions().await.len();
            let adverts = build_endpoint_list(
                &ctx.listeners,
                BootstrapPath::Uds,
                ctx.public_host.as_deref(),
            );
            let endpoints = adverts
                .into_iter()
                .map(|a| EndpointEntry {
                    kind: format!("{}", a.kind),
                    address: a.address,
                })
                .collect();
            let response = StatusResponse {
                status: "running".to_string(),
                port: ctx.quic_port,
                tcp_port: ctx.tcp_port,
                token: ctx.token.clone(),
                pid: std::process::id(),
                uptime_secs: ctx.start_time.elapsed().as_secs(),
                session_count,
                protocol_version: kmux_protocol::messages::PROTOCOL_VERSION,
                kmuxd_version: env!("CARGO_PKG_VERSION").to_string(),
                endpoints,
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
            let response = StopResponse {
                status: "ok".to_string(),
            };
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
            ctx.shutdown.notify_waiters();
        }
        "sessions" => {
            let resp = ctx.app.snapshot_sessions_with_connections().await;
            let mut json = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize sessions response: {e}");
                    return;
                }
            };
            json.push('\n');
            if let Err(e) = write_half.write_all(json.as_bytes()).await {
                warn!("Control socket write error: {e}");
            }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::sync::Notify;

    use kmux_protocol::control_rpc::SessionsResponse;

    use super::{ControlSocketParams, serve_control_socket};
    use crate::app::ServerApp;

    #[tokio::test]
    async fn sessions_command_roundtrips_empty_response() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("ctrl.sock");
        let pid_path = tmp.path().join("daemon.pid");

        let app = Arc::new(ServerApp::new("test-token".to_string()));
        let shutdown = Arc::new(Notify::new());

        let params = ControlSocketParams {
            socket_path: socket_path.clone(),
            pid_path: pid_path.clone(),
            quic_port: 0,
            tcp_port: 0,
            token: "test-token".to_string(),
            start_time: Instant::now(),
            app,
            shutdown: Arc::clone(&shutdown),
            listeners: vec![],
            public_host: None,
        };

        tokio::spawn(serve_control_socket(params));
        // Allow the socket task to bind before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(b"{\"command\":\"sessions\"}\n")
            .await
            .unwrap();

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .expect("timed out")
        .unwrap();

        let resp: SessionsResponse = serde_json::from_str(line.trim()).unwrap();
        assert!(resp.sessions.is_empty());
        assert!(resp.unattached.is_empty());

        shutdown.notify_waiters();
    }
}
