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
        if !handoff_successor && socket_is_live(&socket_path) {
            error!(
                "another daemon is already listening on {}; refusing to start. \
                 Use `kmux daemon stop` or `kmux daemon restart`.",
                socket_path.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&socket_path);
    }

    // Declared *before* the listener so it drops *after* it. That ordering is
    // what makes the guard's "is anything listening?" question meaningful on the
    // way out: our own listener is already closed, so an answer is a
    // successor's. It stays disarmed until the bind below succeeds — until then
    // there is nothing of ours at either path.
    let mut _guard = SocketGuard {
        socket_path: socket_path.clone(),
        pid: pid_file_pid(&pid_path),
        pid_path: pid_path.clone(),
        armed: false,
    };

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
    _guard.armed = true;
    info!("Control socket listening on {}", socket_path.display());

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
/// unreachable. Ownership is therefore established by asking, not by comparing
/// identifiers:
///
/// * the **socket** — is anything listening on it? By the time this runs our own
///   listener is closed (the guard is declared *before* it, so it drops
///   *after*), which is what makes the answer mean something: an answer now is
///   a successor's. Comparing device+inode instead does not work twice over — a
///   filesystem reuses an inode as soon as its file is unlinked, and a bound
///   socket's descriptor `fstat`s to the socket object, not to the filesystem
///   node at the path.
/// * the **pid file** — does it still hold our pid? It holds one number, and
///   reading it back is exactly the check a would-be reader performs.
struct SocketGuard {
    socket_path: PathBuf,
    pid_path: PathBuf,
    /// The pid this daemon wrote, if it wrote one.
    pid: Option<u32>,
    /// Set once the socket is bound. Until then there is nothing of ours at
    /// either path and the guard must keep its hands off.
    armed: bool,
}

/// The pid a pid file holds, if it holds one.
fn pid_file_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if !socket_is_live(&self.socket_path) {
            let _ = std::fs::remove_file(&self.socket_path);
        }
        if self.pid.is_some() && pid_file_pid(&self.pid_path) == self.pid {
            let _ = std::fs::remove_file(&self.pid_path);
        }
    }
}

/// Whether a daemon is actually listening on `path`.
///
/// A connect that succeeds means someone is accepting; `ECONNREFUSED` means the
/// file outlived its daemon. Any other error is treated as live, because
/// removing a socket we could not prove dead is the failure with teeth.
///
/// Blocking, deliberately: both callers are lifecycle moments (binding at
/// startup, cleaning up on exit) and one of them is a `Drop`, which cannot
/// await. Connecting to a Unix socket does not wait on the network.
fn socket_is_live(path: &Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
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

    use std::path::Path;

    use super::{
        ControlSocketParams, SocketGuard, pid_file_pid, serve_control_socket, socket_is_live,
    };
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

    fn guard_for(socket: &Path, pid_path: &Path, pid: u32) -> SocketGuard {
        std::fs::write(pid_path, format!("{pid}\n")).expect("write pid file");
        SocketGuard {
            socket_path: socket.to_path_buf(),
            pid: pid_file_pid(pid_path),
            pid_path: pid_path.to_path_buf(),
            armed: true,
        }
    }

    /// The graceful-restart race: the successor binds the same path while the
    /// predecessor is still winding down. A guard that unlinks by path alone
    /// deletes the *successor's* socket and leaves the new daemon unreachable.
    ///
    /// Identity cannot be device+inode. A filesystem reuses an inode as soon as
    /// its file is unlinked, so the successor's socket can land on the very
    /// number the predecessor recorded — which is what happened on CI when this
    /// compared them, and passed locally. Nor can it be the listener's own
    /// descriptor: a bound Unix socket `fstat`s to the socket object, on a
    /// different device from the filesystem node at its path.
    #[test]
    fn a_guard_does_not_remove_a_socket_a_successor_is_listening_on() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        let guard = guard_for(&socket, &pid, 111);

        // The successor takes over: binds the path and claims the pid file.
        let _successor = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        std::fs::write(&pid, "222\n").expect("rewrite");

        drop(guard);
        assert!(socket.exists(), "the successor's socket must survive");
        assert_eq!(
            pid_file_pid(&pid),
            Some(222),
            "and so must its pid file, untouched"
        );
    }

    /// The ordinary case: our listener is gone and nothing took its place, so
    /// both files are stale and ours to remove.
    #[test]
    fn a_guard_removes_the_socket_and_pid_file_it_bound() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        let guard = guard_for(&socket, &pid, 111);

        // Bound and then closed, exactly as on the way out.
        drop(std::os::unix::net::UnixListener::bind(&socket).expect("bind"));

        drop(guard);
        assert!(!socket.exists());
        assert!(!pid.exists());
    }

    /// A guard that never got as far as binding must not delete whatever it
    /// finds at those paths.
    #[test]
    fn a_disarmed_guard_removes_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        std::fs::write(&socket, b"someone else").expect("write");
        std::fs::write(&pid, b"9").expect("write");

        drop(SocketGuard {
            socket_path: socket.clone(),
            pid: None,
            pid_path: pid.clone(),
            armed: false,
        });
        assert!(socket.exists(), "not ours, not ours to remove");
        assert!(pid.exists());
    }

    /// A pid file another daemon has since claimed stays put even when the
    /// socket half is stale.
    #[test]
    fn a_guard_leaves_a_pid_file_another_daemon_claimed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let socket = tmp.path().join("ctrl.sock");
        let pid = tmp.path().join("daemon.pid");
        let guard = guard_for(&socket, &pid, 111);
        std::fs::write(&pid, "222\n").expect("rewrite");

        drop(guard);
        assert_eq!(pid_file_pid(&pid), Some(222));
    }

    #[test]
    fn a_socket_with_nothing_behind_it_is_not_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A plain file at the socket path: connecting to it cannot succeed, and
        // it is exactly what a killed daemon leaves behind.
        let stale = tmp.path().join("stale.sock");
        std::fs::write(&stale, b"").expect("write");
        assert!(!socket_is_live(&stale));
    }

    #[test]
    fn a_socket_someone_is_listening_on_is_live() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("live.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        assert!(socket_is_live(&path));
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
        assert!(
            socket_is_live(&socket_path),
            "and must still be the incumbent's, not a replacement"
        );
        drop(incumbent);
    }
}
