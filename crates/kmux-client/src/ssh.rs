use std::process::Stdio;

use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tracing::{debug, info};

use crate::hosts::{HostEntry, HostsConfig};

/// Parsed representation of a remote target like `user@host` or a hosts.toml alias.
#[derive(Debug, Clone)]
pub struct RemoteTarget {
    pub user: Option<String>,
    pub host: String,
    pub ssh_port: Option<u16>,
}

/// An active SSH connection with a local TCP tunnel forwarding to the daemon.
pub struct SshSession {
    /// Ephemeral auth token obtained from `kmuxd probe-or-start`.
    pub token: String,
    /// Remote QUIC port (used for upgrade probes).
    pub quic_port: u16,
    /// Remote TCP port on the daemon.
    pub remote_tcp_port: u16,
    /// Local port of the SSH `-L` tunnel, connected to `remote_tcp_port`.
    pub local_tcp_port: u16,
    /// The remote hostname (used for QUIC upgrade probes).
    pub remote_host: String,
    /// Background SSH `-L -N` process; must stay alive for the tunnel to work.
    pub tunnel_process: tokio::process::Child,
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("SSH connection failed: {0}")]
    ConnectionFailed(String),
    #[error("kmuxd is not installed on the remote host")]
    DaemonNotInstalled,
    #[error("failed to start remote daemon: {0}")]
    DaemonStartFailed(String),
    #[error("timed out waiting for the remote daemon to start")]
    DaemonTimeout,
    #[error("SSH tunnel setup failed: {0}")]
    TunnelFailed(String),
    #[error("SSH process exited unexpectedly")]
    SshProcessDied,
}

/// Parse `server` into a `RemoteTarget`.
///
/// Returns `Some` when the string looks like a remote target (contains `@`,
/// or matches a hosts.toml alias that has a `user` configured).
/// Returns `None` for bare `host:port` strings (legacy direct-QUIC mode).
pub fn parse_remote_target(server: &str) -> Option<RemoteTarget> {
    // Explicit user@host syntax.
    if let Some((user, host)) = server.split_once('@') {
        let config = HostsConfig::load();
        let entry = config.get(host).cloned().unwrap_or_default();
        return Some(RemoteTarget {
            user: Some(user.to_string()),
            host: entry.hostname.unwrap_or_else(|| host.to_string()),
            ssh_port: entry.ssh_port,
        });
    }

    // Check hosts.toml alias (no '@').
    let config = HostsConfig::load();
    if let Some(entry) = config.get(server) {
        // Only treat it as SSH mode if a user is configured.
        if entry.user.is_some() {
            return Some(RemoteTarget {
                user: entry.user.clone(),
                host: entry.hostname.clone().unwrap_or_else(|| server.to_string()),
                ssh_port: entry.ssh_port,
            });
        }
    }

    None
}

/// Apply per-host overrides from `hosts.toml` (ssh_port, user, hostname).
pub fn apply_host_overrides(target: &mut RemoteTarget) {
    let config = HostsConfig::load();
    if let Some(entry) = config.get(&target.host) {
        apply_entry(target, entry);
    }
}

fn apply_entry(target: &mut RemoteTarget, entry: &HostEntry) {
    if let Some(h) = &entry.hostname {
        target.host = h.clone();
    }
    if entry.user.is_some() && target.user.is_none() {
        target.user = entry.user.clone();
    }
    if target.ssh_port.is_none() {
        target.ssh_port = entry.ssh_port;
    }
}

/// SSH-negotiate with `target`, returning an `SshSession` ready for TCP use.
///
/// Steps:
/// 1. Run `ssh user@host kmuxd probe-or-start` to get JSON connection info.
/// 2. Spawn `ssh -L 0:127.0.0.1:{tcp_port} -N user@host` for tunnelling.
/// 3. Parse the allocated local port from the tunnel's stderr.
pub async fn negotiate(target: &RemoteTarget) -> Result<SshSession, SshError> {
    let ssh_dest = ssh_destination(target);

    // ── Step 1: probe-or-start ────────────────────────────────────────────────
    debug!(dest = %ssh_dest, "Running kmuxd probe-or-start");
    let probe_output = build_ssh_cmd(target, &ssh_dest)
        .arg("kmuxd")
        .arg("probe-or-start")
        .output()
        .await
        .map_err(|e| SshError::ConnectionFailed(e.to_string()))?;

    if probe_output.status.code() == Some(127) {
        return Err(SshError::DaemonNotInstalled);
    }
    if !probe_output.status.success() {
        let stderr = String::from_utf8_lossy(&probe_output.stderr);
        return Err(SshError::DaemonStartFailed(stderr.into_owned()));
    }

    let stdout = String::from_utf8_lossy(&probe_output.stdout);
    let info: ProbeInfo = serde_json::from_str(stdout.trim())
        .map_err(|e| SshError::DaemonStartFailed(format!("bad JSON from probe-or-start: {e}")))?;

    info!(
        dest = %ssh_dest,
        quic_port = info.quic_port,
        tcp_port = info.tcp_port,
        "Remote daemon ready"
    );

    // ── Step 2: SSH -L tunnel ─────────────────────────────────────────────────
    // Use port 0 so the OS assigns a free local port.  We detect the actual
    // port from the tunnel process's stderr via `-v` (OpenSSH logs it as
    // "Local forwarding listening on 127.0.0.1 port XXXXX.").
    let forward_spec = format!("0:127.0.0.1:{}", info.tcp_port);
    debug!(dest = %ssh_dest, forward = %forward_spec, "Starting SSH -L tunnel");

    let mut tunnel_cmd = build_ssh_cmd(target, &ssh_dest);
    tunnel_cmd
        .arg("-v") // verbose so we can detect the allocated port
        .arg("-L")
        .arg(&forward_spec)
        .arg("-N") // no remote command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .stdin(Stdio::null());

    let mut tunnel = tunnel_cmd
        .spawn()
        .map_err(|e| SshError::TunnelFailed(e.to_string()))?;

    // Read stderr until we find the local port assignment line.
    let local_port =
        detect_local_tunnel_port(tunnel.stderr.take().expect("stderr piped"), info.tcp_port)
            .await
            .map_err(SshError::TunnelFailed)?;

    info!(
        dest = %ssh_dest,
        local_port,
        remote_port = info.tcp_port,
        "SSH tunnel established"
    );

    Ok(SshSession {
        token: info.token,
        quic_port: info.quic_port,
        remote_tcp_port: info.tcp_port,
        local_tcp_port: local_port,
        remote_host: target.host.clone(),
        tunnel_process: tunnel,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ProbeInfo {
    quic_port: u16,
    tcp_port: u16,
    token: String,
}

fn ssh_destination(target: &RemoteTarget) -> String {
    match &target.user {
        Some(u) => format!("{}@{}", u, target.host),
        None => target.host.clone(),
    }
}

/// Build a base `ssh` command with shared flags.
fn build_ssh_cmd(target: &RemoteTarget, dest: &str) -> Command {
    let ssh_bin = std::env::var("KMUX_SSH_BIN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ssh".to_string());

    let mut cmd = Command::new(&ssh_bin);
    cmd.arg("-o").arg("BatchMode=yes"); // non-interactive
    cmd.arg("-o").arg("StrictHostKeyChecking=accept-new");
    if let Some(port) = target.ssh_port {
        cmd.arg("-p").arg(port.to_string());
    }
    cmd.arg(dest);
    cmd
}

/// Read from `stderr` until OpenSSH logs the allocated local port for `-L`.
///
/// OpenSSH verbose output contains a line like:
/// `debug1: Local forwarding listening on 127.0.0.1 port 54321.`
async fn detect_local_tunnel_port(
    stderr: tokio::process::ChildStderr,
    _remote_port: u16,
) -> Result<u16, String> {
    use tokio::time::{Duration, timeout};

    let reader = tokio::io::BufReader::new(stderr);
    let mut lines = reader.lines();

    let deadline = Duration::from_secs(15);

    timeout(deadline, async {
        while let Ok(Some(line)) = lines.next_line().await {
            debug!(line = %line, "ssh tunnel stderr");
            // OpenSSH -v outputs: "debug1: Local forwarding listening on 127.0.0.1 port NNNNN."
            if let Some(port) = parse_listening_port(&line) {
                return Ok(port);
            }
        }
        Err("SSH stderr closed without reporting local tunnel port".to_string())
    })
    .await
    .map_err(|_| "Timed out waiting for SSH tunnel port assignment".to_string())?
}

/// Extract the port number from OpenSSH verbose output lines like:
/// `debug1: Local forwarding listening on 127.0.0.1 port 54321.`
fn parse_listening_port(line: &str) -> Option<u16> {
    // Look for "port " followed by digits at/near end-of-line.
    let marker = "port ";
    let pos = line.find(marker)?;
    let after = &line[pos + marker.len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_target_user_at_host() {
        let t = parse_remote_target("alice@example.com").unwrap();
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "example.com");
        assert!(t.ssh_port.is_none());
    }

    #[test]
    fn parse_remote_target_bare_host_no_ssh() {
        // No '@', no hosts.toml entry → should return None
        let result = parse_remote_target("192.168.1.1:7777");
        assert!(result.is_none());
    }

    #[test]
    fn parse_listening_port_extracts_port() {
        let line = "debug1: Local forwarding listening on 127.0.0.1 port 54321.";
        assert_eq!(parse_listening_port(line), Some(54321));
    }

    #[test]
    fn parse_listening_port_no_match() {
        assert_eq!(parse_listening_port("some other ssh output"), None);
    }
}
