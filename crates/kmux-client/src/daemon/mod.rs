mod lifecycle;
pub use lifecycle::ensure_daemon;

/// Protects XDG_RUNTIME_DIR mutations — shared across all daemon tests.
#[cfg(test)]
pub(super) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Connection parameters returned by the running daemon.
pub struct DaemonStatus {
    pub port: u16,
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub session_count: usize,
}

#[derive(Deserialize)]
struct StatusResponse {
    port: u16,
    #[serde(default)]
    tcp_port: u16,
    token: String,
    pid: u32,
    #[serde(default)]
    uptime_secs: u64,
    #[serde(default)]
    session_count: usize,
}

/// Query a running daemon via its Unix control socket.
///
/// Returns `None` if the daemon is not reachable, not responding, or the PID
/// reported by the daemon is no longer alive.
pub async fn query_daemon() -> Option<DaemonStatus> {
    let socket_path = kmux_protocol::dirs::socket_path().ok()?;

    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
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

    let resp: StatusResponse = serde_json::from_str(line.trim()).ok()?;

    // Verify the reported PID is actually alive.
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
///
/// Returns `Ok(())` after the daemon acknowledges the request. Returns an error
/// if the daemon is not reachable (not running or socket unavailable).
pub async fn stop_daemon() -> anyhow::Result<()> {
    let socket_path = kmux_protocol::dirs::socket_path()
        .map_err(|e| anyhow::anyhow!("could not resolve socket path: {e}"))?;

    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
        .await
        .map_err(|_| anyhow::anyhow!("daemon is not running"))?
        .map_err(|_| anyhow::anyhow!("daemon is not running"))?;

    let (read_half, mut write_half) = stream.into_split();

    write_half
        .write_all(b"{\"command\":\"stop\"}\n")
        .await
        .map_err(|e| anyhow::anyhow!("failed to send stop command: {e}"))?;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("daemon did not respond to stop command"))?
        .map_err(|e| anyhow::anyhow!("failed to read stop response: {e}"))?;

    #[derive(serde::Deserialize)]
    struct StopResponse {
        status: String,
    }
    let resp: StopResponse = serde_json::from_str(line.trim())
        .map_err(|e| anyhow::anyhow!("invalid stop response: {e}"))?;
    if resp.status != "ok" {
        return Err(anyhow::anyhow!("unexpected stop response: {}", resp.status));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn query_daemon_parses_session_count() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", tmp.path()) };

        let socket_path = kmux_protocol::dirs::socket_path().unwrap();
        let listener = UnixListener::bind(&socket_path).unwrap();

        // Use current PID so pid_alive() passes in all environments.
        let my_pid = std::process::id();

        // Fake daemon: read the client request, then respond with status JSON.
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

        // Give the listener a moment to be ready.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let status = query_daemon().await;
        // PID 1 is always alive (init), so pid_alive check passes.
        let status = status.expect("expected Some from mock daemon");
        assert_eq!(status.port, 9999);
        assert_eq!(status.uptime_secs, 42);
        assert_eq!(status.session_count, 3);
    }
}
