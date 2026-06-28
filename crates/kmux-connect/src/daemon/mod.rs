mod lifecycle;
pub use lifecycle::ensure_daemon;
pub(crate) use lifecycle::find_server_binary;
pub use lifecycle::{force_kill_daemon, pid_alive, running_daemon_pid, wait_for_exit};

/// Resolve the `kmuxd` binary an auto-spawn would launch, using the same
/// precedence as the spawn path (`KMUX_KMUXD` → exe sibling → debug
/// `target/<profile>` → `$PATH`). Exposed for diagnostics (`kmux debug paths`)
/// so a developer can see *which* daemon a connect would start.
pub fn resolve_kmuxd_path() -> anyhow::Result<std::path::PathBuf> {
    find_server_binary()
}

/// The tail of the `kmuxd-boot.log`, formatted as an error suffix (or `""` when
/// empty). Exposed so `kmux daemon restart` can explain a timeout by showing
/// why a freshly-spawned daemon never came up — e.g. `No space left on device`
/// — instead of a blind "timed out" with no cause.
pub fn boot_log_hint() -> String {
    lifecycle::format_boot_log_hint()
}

/// Protects XDG_RUNTIME_DIR mutations — shared across all daemon tests.
#[cfg(test)]
pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use kmux_protocol::control_rpc::{SessionsResponse, StatusResponse};
use kmux_protocol::dirs::BuildProfile;

/// Connection parameters returned by the running daemon.
#[derive(Debug)]
pub struct DaemonStatus {
    pub port: u16,
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub session_count: usize,
    pub protocol_version: u32,
    pub kmuxd_version: String,
    /// Build fingerprint of the running daemon, `<sha>[-dirty]` (empty when the
    /// daemon predates this field). Compared against the client/CLI build to
    /// surface skew that a matching protocol version alone cannot.
    pub kmuxd_build: String,
    /// `None` when the daemon predates this field — treated as
    /// unverifiable and therefore rejected by `ensure_compatible_daemon`.
    pub build_profile: Option<BuildProfile>,
}

/// Send a single JSON command to the daemon control socket and parse the response.
///
/// Returns an error if the daemon is unreachable, times out, or the response
/// cannot be deserialized into `Resp`.
async fn control_request<Resp: DeserializeOwned>(command: &str) -> anyhow::Result<Resp> {
    let socket_path = kmux_protocol::dirs::socket_path()
        .map_err(|e| anyhow::anyhow!("could not resolve socket path: {e}"))?;

    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
        .await
        .map_err(|_| anyhow::anyhow!("daemon is not running (connection timed out)"))?
        .map_err(|_| anyhow::anyhow!("daemon is not running"))?;

    let (read_half, mut write_half) = stream.into_split();

    let request = format!("{{\"command\":\"{command}\"}}\n");
    write_half
        .write_all(request.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("failed to send command: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not respond in time"))?
        .map_err(|e| anyhow::anyhow!("failed to read response: {e}"))?;

    serde_json::from_str(line.trim())
        .map_err(|e| anyhow::anyhow!("invalid response from daemon: {e}"))
}

/// Query a running daemon via its Unix control socket.
///
/// Returns `None` if the daemon is not reachable, not responding, or the PID
/// reported by the daemon is no longer alive.
pub async fn query_daemon() -> Option<DaemonStatus> {
    let resp: StatusResponse = control_request("status").await.ok()?;

    if !lifecycle::pid_alive(resp.pid) {
        return None;
    }

    Some(DaemonStatus {
        port: resp.port,
        tcp_port: resp.tcp_port,
        token: resp.token,
        pid: resp.pid,
        uptime_secs: resp.uptime_secs,
        session_count: resp.session_count,
        protocol_version: resp.protocol_version,
        kmuxd_version: resp.kmuxd_version,
        kmuxd_build: resp.kmuxd_build,
        build_profile: resp.build_profile,
    })
}

/// Ensure a local daemon is running and that its protocol version matches ours.
///
/// Starts the daemon if it is not running, then verifies the version reported
/// via the control socket. Returns `Err` immediately when there is a version
/// mismatch — the caller must not attempt a data-plane connection.
///
/// Use this instead of `ensure_daemon()` for every connection path that talks
/// to the local daemon.
pub async fn ensure_compatible_daemon() -> anyhow::Result<DaemonStatus> {
    use kmux_protocol::messages::PROTOCOL_VERSION;

    let status = lifecycle::ensure_daemon().await?;

    if status.protocol_version != 0 && status.protocol_version != PROTOCOL_VERSION {
        let hint = if status.protocol_version < PROTOCOL_VERSION {
            "Hint: the running kmuxd is older than kmux. Run `kmux daemon restart` to update it."
        } else {
            "Hint: the running kmuxd is newer than kmux. Update the kmux client to match."
        };
        anyhow::bail!(
            "protocol version mismatch: client={}, daemon={} ({})\n{}",
            PROTOCOL_VERSION,
            status.protocol_version,
            status.kmuxd_version,
            hint
        );
    }

    let socket = || {
        kmux_protocol::dirs::socket_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string())
    };
    match status.build_profile {
        Some(p) if p == BuildProfile::CURRENT => {}
        Some(p) => anyhow::bail!(
            "build profile mismatch: kmux is {client} but the daemon answering on \
             {socket} is {daemon}. Debug and release builds keep separate runtime \
             dirs, so the two never share sockets — run the matching kmux binary \
             or restart the daemon with a matching build.",
            client = BuildProfile::CURRENT,
            daemon = p,
            socket = socket(),
        ),
        None => anyhow::bail!(
            "daemon on {socket} did not report a build profile; refusing to attach \
             because we cannot verify it matches kmux ({client}). Restart the \
             daemon with a current kmuxd build.",
            client = BuildProfile::CURRENT,
            socket = socket(),
        ),
    }

    Ok(status)
}

/// Request a graceful shutdown by sending `stop` to the daemon control socket.
///
/// This only *asks* the daemon to shut down — the `"ok"` reply is sent before
/// the process actually exits, so it is **not** proof of termination. Callers
/// that need to confirm the daemon is gone must follow up with
/// [`wait_for_exit`] (and escalate via [`force_kill_daemon`] on timeout).
pub async fn stop_daemon() -> anyhow::Result<()> {
    use kmux_protocol::control_rpc::StopResponse;
    let resp: StopResponse = control_request("stop").await?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!("unexpected stop response: {}", resp.status));
    }
    Ok(())
}

/// Ask the running daemon to perform a graceful live-PTY handoff to a successor.
///
/// Returns `Ok(true)` when the daemon accepted the handoff (running shells will
/// migrate), `Ok(false)` when it reports `busy`, and `Err(_)` when the daemon is
/// too old to understand `restart` (it closes the connection without replying,
/// so the response cannot be read) or is unreachable. The caller falls back to a
/// hard stop-then-respawn restart in those cases.
pub async fn restart_daemon() -> anyhow::Result<bool> {
    use kmux_protocol::control_rpc::RestartResponse;
    let resp: RestartResponse = control_request("restart").await?;
    Ok(resp.status == "ok")
}

/// Query the daemon for its active sessions and per-connection metrics.
pub async fn query_daemon_sessions() -> anyhow::Result<SessionsResponse> {
    control_request("sessions").await
}

/// Query the daemon for every live client connection with its build identity
/// (protocol 37). Used by `kmux client status` to find the local GUI client's
/// connection and compare its build against the daemon's.
pub async fn query_connections() -> anyhow::Result<kmux_protocol::control_rpc::ConnectionsResponse>
{
    control_request("connections").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn query_daemon_parses_session_count() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        let my_pid = std::process::id();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let response = format!(
                    "{{\"status\":\"running\",\"port\":9999,\"token\":\"tok\",\
                     \"pid\":{my_pid},\"uptime_secs\":42,\"session_count\":3,\
                     \"protocol_version\":{},\"kmuxd_version\":\"0.0.0\"}}\n",
                    kmux_protocol::messages::PROTOCOL_VERSION
                );
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let status = query_daemon().await;
        let status = status.expect("expected Some from mock daemon");
        assert_eq!(status.port, 9999);
        assert_eq!(status.uptime_secs, 42);
        assert_eq!(status.session_count, 3);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn query_daemon_sessions_roundtrip() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                // Verify the command name was forwarded correctly.
                assert!(line.contains("\"sessions\""));
                let response = r#"{"sessions":[],"unattached":[]}"#.to_string() + "\n";
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let resp = query_daemon_sessions().await.expect("should parse");
        assert!(resp.sessions.is_empty());
        assert!(resp.unattached.is_empty());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ensure_compatible_daemon_rejects_version_mismatch() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        let my_pid = std::process::id();
        // Respond with a mismatched protocol_version.
        let stale_version = kmux_protocol::messages::PROTOCOL_VERSION.wrapping_sub(1);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let response = format!(
                    "{{\"status\":\"running\",\"port\":9999,\"token\":\"tok\",\
                     \"pid\":{my_pid},\"uptime_secs\":0,\"session_count\":0,\
                     \"protocol_version\":{stale_version},\"kmuxd_version\":\"0.0.0\"}}\n"
                );
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // ensure_compatible_daemon must error, not return Ok.
        let result = super::ensure_compatible_daemon().await;
        assert!(result.is_err(), "expected Err on version mismatch");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("protocol version mismatch"),
            "error should mention mismatch: {msg}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ensure_compatible_daemon_rejects_build_profile_mismatch() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        let my_pid = std::process::id();
        // Flip the profile so it never matches whatever the test binary was
        // compiled with.
        let wrong_profile = match BuildProfile::CURRENT {
            BuildProfile::Debug => "release",
            BuildProfile::Release => "debug",
        };
        let proto = kmux_protocol::messages::PROTOCOL_VERSION;

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let response = format!(
                    "{{\"status\":\"running\",\"port\":9999,\"token\":\"tok\",\
                     \"pid\":{my_pid},\"uptime_secs\":0,\"session_count\":0,\
                     \"protocol_version\":{proto},\"kmuxd_version\":\"0.0.0\",\
                     \"build_profile\":\"{wrong_profile}\"}}\n"
                );
                write_half.write_all(response.as_bytes()).await.unwrap();
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let result = super::ensure_compatible_daemon().await;
        assert!(result.is_err(), "expected Err on build profile mismatch");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("build profile mismatch"),
            "error should mention build profile mismatch: {msg}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn control_request_timeout_surfaces_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        // No listener — should return an error, not hang.
        let result: anyhow::Result<StatusResponse> = control_request("status").await;
        assert!(result.is_err());
    }

    /// The `restart` control RPC has three outcomes the `kmux daemon restart`
    /// command branches on (see `kmux-app/src/subcommands/daemon_cmd.rs`):
    ///   - `{"status":"ok"}`   → `Ok(true)`  — graceful handoff accepted
    ///   - `{"status":"busy"}` → `Ok(false)` — a restart is already in progress
    ///   - connection closed without a reply → `Err` — daemon predates `restart`,
    ///     so the caller falls back to a hard stop-then-respawn.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn restart_daemon_maps_accepted_busy_and_unsupported() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Serve three connections in order: accept, busy, then close-without-reply.
        tokio::spawn(async move {
            let replies: [Option<&str>; 3] = [
                Some(r#"{"status":"ok","handoff":true}"#),
                Some(r#"{"status":"busy","handoff":false}"#),
                None, // mimic an old daemon: close without replying
            ];
            for reply in replies {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                assert!(line.contains("\"restart\""), "expected a restart command");
                if let Some(body) = reply {
                    let _ = write_half.write_all(format!("{body}\n").as_bytes()).await;
                }
                // Dropping `write_half` (reply == None) closes the connection so the
                // client reads EOF and surfaces an error.
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert!(
            super::restart_daemon()
                .await
                .expect("accepted reply parses"),
            "status=ok must report an accepted handoff"
        );
        assert!(
            !super::restart_daemon().await.expect("busy reply parses"),
            "status=busy must report no handoff"
        );
        assert!(
            super::restart_daemon().await.is_err(),
            "a daemon that closes without replying must surface as Err (unsupported)"
        );
    }
}
