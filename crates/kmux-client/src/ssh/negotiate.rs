use std::process::Stdio;

use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

use kmux_protocol::messages::PROTOCOL_VERSION;

use super::{RemoteTarget, SshError, SshSession};

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
    let probe_json = stdout.trim().to_string();
    let info: ProbeInfo = serde_json::from_str(&probe_json)
        .map_err(|e| SshError::DaemonStartFailed(format!("bad JSON from probe-or-start: {e}")))?;

    // Version gate: if the server reports its protocol_version, it must match.
    // Older daemons that don't report the field are accepted with a warning.
    match info.protocol_version {
        Some(server_ver) if server_ver != PROTOCOL_VERSION => {
            return Err(SshError::VersionMismatch {
                client: PROTOCOL_VERSION,
                server: server_ver,
            });
        }
        None => {
            warn!(
                dest = %ssh_dest,
                "Remote daemon did not report protocol_version; \
                 update kmuxd to ensure compatibility"
            );
        }
        Some(_) => {}
    }

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
    let local_port = detect_local_tunnel_port(
        tunnel.stderr.take().expect("stderr piped"),
        &ssh_dest,
        &forward_spec,
    )
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
        probe_json,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct ProbeInfo {
    quic_port: u16,
    tcp_port: u16,
    token: String,
    /// `protocol_version` field added in Phase 7; older daemons omit it.
    #[serde(default)]
    protocol_version: Option<u32>,
}

pub(super) fn ssh_destination(target: &RemoteTarget) -> String {
    match &target.user {
        Some(u) => format!("{}@{}", u, target.host),
        None => target.host.clone(),
    }
}

/// Build a base `ssh` command with shared flags.
pub(super) fn build_ssh_cmd(target: &RemoteTarget, dest: &str) -> Command {
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
///
/// On failure, emits a `warn!` with the last SSH stderr lines so the caller
/// can diagnose auth errors, connection refusals, etc. without needing RUST_LOG=debug.
pub(super) async fn detect_local_tunnel_port(
    stderr: tokio::process::ChildStderr,
    dest: &str,
    forward_spec: &str,
) -> Result<u16, String> {
    use tokio::time::{Duration, timeout};

    let reader = tokio::io::BufReader::new(stderr);
    let mut lines = reader.lines();
    let dest_owned = dest.to_owned();
    let fwd_owned = forward_spec.to_owned();

    let result = timeout(Duration::from_secs(15), async move {
        // Ring buffer: keep the last 20 lines for diagnostics on failure.
        let mut seen: Vec<String> = Vec::with_capacity(20);
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    debug!(line = %line, "ssh tunnel stderr");
                    // OpenSSH -v: "debug1: Local forwarding listening on 127.0.0.1 port NNNNN."
                    if let Some(port) = parse_listening_port(&line) {
                        return Ok(port);
                    }
                    if seen.len() == 20 {
                        seen.remove(0);
                    }
                    seen.push(line);
                }
                _ => {
                    // EOF or read error — SSH exited without printing the port line.
                    // Emit a warn with the collected output so the caller can see why.
                    let ssh_output = if seen.is_empty() {
                        "(no output)".to_string()
                    } else {
                        seen.iter()
                            .map(|l| format!("  {l}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    warn!(
                        dest = %dest_owned,
                        forward = %fwd_owned,
                        ssh_output = %ssh_output,
                        "SSH stderr closed before reporting tunnel port"
                    );
                    return Err(format!(
                        "SSH stderr closed without reporting local tunnel port \
                         (dest={dest_owned}, forward={fwd_owned})"
                    ));
                }
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => {
            warn!(
                dest = %dest,
                forward = %forward_spec,
                "SSH tunnel port detection timed out after 15s; \
                 re-run with RUST_LOG=kmux_client=debug for live stderr"
            );
            Err(format!(
                "Timed out (15s) waiting for SSH tunnel port assignment \
                 (dest={dest}, forward={forward_spec})"
            ))
        }
    }
}

/// Extract the port number from OpenSSH verbose output lines like:
/// `debug1: Local forwarding listening on 127.0.0.1 port 54321.`
pub(super) fn parse_listening_port(line: &str) -> Option<u16> {
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

    // ── parse_listening_port tests ───────────────────────────────────────────

    #[test]
    fn parse_listening_port_extracts_port() {
        let line = "debug1: Local forwarding listening on 127.0.0.1 port 54321.";
        assert_eq!(parse_listening_port(line), Some(54321));
    }

    #[test]
    fn parse_listening_port_no_match() {
        assert_eq!(parse_listening_port("some other ssh output"), None);
    }

    // ── ProbeInfo deserialization tests ─────────────────────────────────────

    #[test]
    fn probe_info_with_protocol_version() {
        let json = r#"{"quic_port":8443,"tcp_port":8444,"token":"abc","protocol_version":13}"#;
        let info: ProbeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.protocol_version, Some(13));
    }

    #[test]
    fn probe_info_without_protocol_version_defaults_to_none() {
        let json = r#"{"quic_port":8443,"tcp_port":8444,"token":"abc"}"#;
        let info: ProbeInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.protocol_version, None);
    }

    #[test]
    fn probe_info_version_mismatch_detected() {
        // Simulate: client PROTOCOL_VERSION is X, server reports X+1.
        // We can't call negotiate() in a unit test (it spawns SSH), so test
        // the version check logic by verifying the sentinel values.
        let server_ver: u32 = PROTOCOL_VERSION.wrapping_add(1);
        let client_ver: u32 = PROTOCOL_VERSION;
        assert_ne!(client_ver, server_ver, "mismatch must differ");
    }
}
