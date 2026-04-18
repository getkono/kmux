mod lifecycle;
pub use lifecycle::ensure_daemon;
pub(crate) use lifecycle::find_server_binary;

/// Protects XDG_RUNTIME_DIR mutations — shared across all daemon tests.
#[cfg(test)]
pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use kmux_protocol::control_rpc::{SessionsResponse, StatusResponse};

/// Connection parameters returned by the running daemon.
pub struct DaemonStatus {
    pub port: u16,
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub session_count: usize,
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
    })
}

/// Send a stop command to the running daemon via its Unix control socket.
pub async fn stop_daemon() -> anyhow::Result<()> {
    use kmux_protocol::control_rpc::StopResponse;
    let resp: StopResponse = control_request("stop").await?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!("unexpected stop response: {}", resp.status));
    }
    Ok(())
}

/// Query the daemon for its active sessions and per-connection metrics.
pub async fn query_daemon_sessions() -> anyhow::Result<SessionsResponse> {
    control_request("sessions").await
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
                     \"pid\":{my_pid},\"uptime_secs\":42,\"session_count\":3}}\n"
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
    async fn control_request_timeout_surfaces_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        // No listener — should return an error, not hang.
        let result: anyhow::Result<StatusResponse> = control_request("status").await;
        assert!(result.is_err());
    }
}
