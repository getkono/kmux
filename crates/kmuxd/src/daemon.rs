use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::announce::{BootstrapPath, build_endpoint_list};
use crate::app::ServerApp;
use crate::config::ListenConfig;
use kmux_protocol::control_rpc::{
    ControlRequest, EndpointEntry, RestartResponse, StatusResponse, StopResponse,
};

/// Daemonize the current process (double-fork), optionally writing+locking the
/// child PID into `pid_path`.
///
/// This MUST be called before any tokio runtime is created — fork and async
/// runtimes do not mix safely.
///
/// `pid_path` is `None` for a graceful-restart successor: the `daemonize` crate
/// `flock`s the pid file, and the predecessor still holds that lock, so a
/// successor must daemonize without it and write the pid file itself once the
/// predecessor has exited (see `startup::async_main`).
pub fn daemonize_process(pid_path: Option<&Path>) -> anyhow::Result<()> {
    use daemonize::{Daemonize, Stdio};

    // Keep inherited stdout/stderr so errors that occur in the daemonized
    // grandchild (e.g. port bind failure, TLS setup error) are written to the
    // boot log that kmux-client opened for us, making them visible in the
    // failure hint instead of disappearing into /dev/null.
    let mut daemonize = Daemonize::new()
        .working_directory("/")
        .umask(0o077)
        .stdout(Stdio::keep())
        .stderr(Stdio::keep());
    if let Some(path) = pid_path {
        daemonize = daemonize.pid_file(path);
    }
    daemonize
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
    /// Signals `async_main` to begin a graceful live-PTY handoff.
    restart: Arc<Notify>,
    /// Guards against concurrent restarts; set while a handoff is in flight.
    handoff_in_progress: Arc<AtomicBool>,
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
    /// Signals `async_main` to begin a graceful live-PTY handoff.
    pub restart: Arc<Notify>,
    /// Guards against concurrent restarts; set while a handoff is in flight.
    pub handoff_in_progress: Arc<AtomicBool>,
    /// Resolved listener configs (with actual bound ports filled in).
    pub listeners: Vec<ListenConfig>,
    pub public_host: Option<String>,
    /// True when this daemon is a graceful-restart successor, which is the one
    /// case where taking over a socket another daemon is still listening on is
    /// correct: the predecessor said `Released` before we got here.
    pub handoff_successor: bool,
}

/// Bind the Unix control socket and serve status queries.
///
/// Removes a *stale* socket file before binding — never a live one, unless this
/// daemon is a handoff successor. Each accepted connection receives exactly one
/// request/response exchange then closes.
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
        restart,
        handoff_in_progress,
        listeners,
        public_host,
        handoff_successor,
    } = params;

    let ctx = RequestCtx {
        quic_port,
        tcp_port,
        token,
        start_time,
        app,
        shutdown: Arc::clone(&shutdown),
        restart,
        handoff_in_progress,
        listeners,
        public_host,
    };
    // A socket file left by a previous run must go before we can bind. A socket
    // file a *live* daemon is listening on must not: unlinking it does not stop
    // that daemon, it only makes it unreachable, and the host is left with two
    // daemons and its sessions split between them. Ask before removing.
    //
    // The handoff successor is the exception, and the only one: the predecessor
    // sent `Released` precisely to say the socket is ours to take.
    if socket_path.exists() {
        if !handoff_successor && socket_is_live(&socket_path).await {
            error!(
                "another daemon is already listening on {}; refusing to start. \
                 Use `kmux daemon stop` or `kmux daemon restart`.",
                socket_path.display()
            );
            return;
        }
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
    let _guard = SocketGuard {
        socket_id: FileId::of(&socket_path),
        socket_path: socket_path.clone(),
        pid_id: FileId::of(&pid_path),
        pid_path: pid_path.clone(),
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
                protocol_version: kmux_protocol::messages::LEGACY_PROTOCOL_VERSION,
                protocol_range: Some(kmux_protocol::messages::PROTOCOL_RANGE),
                kmuxd_version: env!("CARGO_PKG_VERSION").to_string(),
                kmuxd_build: kmux_protocol::buildinfo::fingerprint(),
                build_profile: Some(kmux_protocol::compat::BuildProfile::CURRENT),
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
        "restart" => {
            // Refuse a second concurrent handoff.
            let busy = ctx.handoff_in_progress.swap(true, Ordering::SeqCst);
            let response = RestartResponse {
                status: if busy { "busy" } else { "ok" }.to_string(),
                handoff: true,
            };
            let mut json = match serde_json::to_string(&response) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize restart response: {e}");
                    return;
                }
            };
            json.push('\n');
            if let Err(e) = write_half.write_all(json.as_bytes()).await {
                warn!("Control socket write error: {e}");
            }
            if busy {
                warn!("restart command ignored: a handoff is already in progress");
            } else {
                info!("Received restart command, beginning graceful handoff");
                // Wake the main task; it consumes the in-progress flag and clears
                // it if the handoff rolls back.
                ctx.restart.notify_one();
            }
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
        "connections" => {
            let resp = ctx.app.snapshot_connections().await;
            let mut json = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize connections response: {e}");
                    return;
                }
            };
            json.push('\n');
            if let Err(e) = write_half.write_all(json.as_bytes()).await {
                warn!("Control socket write error: {e}");
            }
        }
        "workers" => {
            let resp = ctx.app.snapshot_workers().await;
            let mut json = match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => {
                    warn!("Failed to serialize workers response: {e}");
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
/// Removes this daemon's socket and pid file on exit — and only ever *this*
/// daemon's.
///
/// A graceful restart has the successor bind the same paths while the
/// predecessor is still winding down, so a guard that unlinks by path alone can
/// delete the file its replacement just created and leave the new daemon
/// unreachable. Recording the identity of what was bound and comparing it on the
/// way out makes that impossible: a path now pointing at someone else's inode is
/// not ours to remove.
struct SocketGuard {
    socket_path: PathBuf,
    socket_id: Option<FileId>,
    pid_path: PathBuf,
    pid_id: Option<FileId>,
}

/// A file's identity on the filesystem: the pair that survives a rename and
/// changes on a replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    /// The identity of whatever `path` names now, or `None` if nothing does.
    fn of(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt as _;
        let md = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: md.dev(),
            ino: md.ino(),
        })
    }
}

/// Remove `path` only if it still names the file identified by `expected`.
fn remove_if_still_ours(path: &Path, expected: Option<FileId>) {
    if expected.is_some() && FileId::of(path) == expected {
        let _ = std::fs::remove_file(path);
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        remove_if_still_ours(&self.socket_path, self.socket_id);
        remove_if_still_ours(&self.pid_path, self.pid_id);
    }
}

/// Whether a daemon is actually listening on `path`.
///
/// A connect that succeeds means someone is accepting; `ECONNREFUSED` means the
/// file outlived its daemon. Any other error is treated as live, because
/// removing a socket we could not prove dead is the failure with teeth.
async fn socket_is_live(path: &Path) -> bool {
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::ConnectionRefused,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Notify;

    use kmux_protocol::control_rpc::SessionsResponse;

    use super::{ControlSocketParams, FileId, SocketGuard, serve_control_socket, socket_is_live};
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
            restart: Arc::new(Notify::new()),
            handoff_in_progress: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            listeners: vec![],
            public_host: None,
            handoff_successor: false,
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

    // ─── Socket ownership ────────────────────────────────────────────────────

    /// The graceful-restart race: the successor binds the same path while the
    /// predecessor is still winding down. A guard that unlinks by path alone
    /// deletes the *successor's* socket and leaves the new daemon unreachable.
    #[test]
    fn a_guard_does_not_remove_a_file_its_successor_replaced() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        std::fs::write(&socket, b"predecessor").expect("write");
        std::fs::write(&pid, b"1").expect("write");

        let guard = SocketGuard {
            socket_id: FileId::of(&socket),
            socket_path: socket.clone(),
            pid_id: FileId::of(&pid),
            pid_path: pid.clone(),
        };

        // The successor replaces both, so the paths now name different inodes.
        std::fs::remove_file(&socket).expect("unlink");
        std::fs::write(&socket, b"successor").expect("rebind");
        std::fs::remove_file(&pid).expect("unlink");
        std::fs::write(&pid, b"2").expect("rewrite");

        drop(guard);
        assert!(socket.exists(), "the successor's socket must survive");
        assert!(pid.exists(), "and so must its pid file");
        assert_eq!(std::fs::read(&socket).expect("read"), b"successor");
    }

    /// The ordinary case: nothing replaced them, so they are ours to clean up.
    #[test]
    fn a_guard_removes_the_files_it_bound() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        std::fs::write(&socket, b"ours").expect("write");
        std::fs::write(&pid, b"1").expect("write");

        drop(SocketGuard {
            socket_id: FileId::of(&socket),
            socket_path: socket.clone(),
            pid_id: FileId::of(&pid),
            pid_path: pid.clone(),
        });
        assert!(!socket.exists());
        assert!(!pid.exists());
    }

    /// A daemon that never bound anything must not delete whatever it finds at
    /// those paths on the way out.
    #[test]
    fn a_guard_that_bound_nothing_removes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");

        let guard = SocketGuard {
            socket_id: FileId::of(&socket), // None: nothing there yet
            socket_path: socket.clone(),
            pid_id: FileId::of(&pid),
            pid_path: pid.clone(),
        };
        // Someone else creates them in the meantime.
        std::fs::write(&socket, b"someone else").expect("write");
        std::fs::write(&pid, b"9").expect("write");

        drop(guard);
        assert!(socket.exists(), "not ours, not ours to remove");
        assert!(pid.exists());
    }

    #[tokio::test]
    async fn a_socket_with_nothing_behind_it_is_not_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A plain file at the socket path: connecting to it cannot succeed, and
        // it is exactly what a killed daemon leaves behind.
        let stale = tmp.path().join("stale.sock");
        std::fs::write(&stale, b"").expect("write");
        assert!(!socket_is_live(&stale).await);
    }

    #[tokio::test]
    async fn a_socket_someone_is_listening_on_is_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("live.sock");
        let _listener = UnixListener::bind(&path).expect("bind");
        assert!(socket_is_live(&path).await);
    }

    /// The headline: a second daemon must not unlink a running daemon's socket.
    /// Doing so does not stop it — it only makes it unreachable, leaving the
    /// host with two daemons and its sessions split between them.
    #[tokio::test]
    async fn a_second_daemon_refuses_to_take_over_a_live_control_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket_path = tmp.path().join("ctrl.sock");
        let pid_path = tmp.path().join("daemon.pid");

        // Stand in for the incumbent: something bound and listening.
        let incumbent = UnixListener::bind(&socket_path).expect("bind");
        let incumbent_id = FileId::of(&socket_path);

        let params = ControlSocketParams {
            socket_path: socket_path.clone(),
            pid_path,
            quic_port: 0,
            tcp_port: 0,
            token: "test-token".to_string(),
            start_time: Instant::now(),
            app: Arc::new(ServerApp::new("test-token".to_string())),
            shutdown: Arc::new(Notify::new()),
            restart: Arc::new(Notify::new()),
            handoff_in_progress: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            listeners: vec![],
            public_host: None,
            handoff_successor: false,
        };
        // Returns rather than binding, and leaves the incumbent's socket alone.
        serve_control_socket(params).await;

        assert!(socket_path.exists(), "the incumbent's socket must survive");
        assert_eq!(
            FileId::of(&socket_path),
            incumbent_id,
            "and must still be the same socket, not a replacement"
        );
        drop(incumbent);
    }
}
