mod negotiate;
pub use negotiate::negotiate;

use crate::hosts::{HostEntry, HostsConfig};

/// Components extracted from a server string like `user@host:/path` or `host:port`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedServer {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub path: Option<String>,
}

/// Parse a server string into its components.
///
/// Supported formats:
/// - `user@host:/path`  — SSH with remote path (`:` + `/` = path, not port)
/// - `user@host:2222`   — SSH with explicit port (`:` + digits = port)
/// - `user@host`        — SSH with default port
/// - `host:port`        — direct QUIC (no `@`)
/// - `alias`            — hosts.toml lookup
pub fn parse_server_string(server: &str) -> ParsedServer {
    if let Some((user, rest)) = server.split_once('@') {
        if let Some((host, after_colon)) = rest.split_once(':') {
            if after_colon.starts_with('/') {
                // user@host:/path
                ParsedServer {
                    user: Some(user.to_string()),
                    host: host.to_string(),
                    port: None,
                    path: Some(after_colon.to_string()),
                }
            } else if let Ok(p) = after_colon.parse::<u16>() {
                // user@host:port
                ParsedServer {
                    user: Some(user.to_string()),
                    host: host.to_string(),
                    port: Some(p),
                    path: None,
                }
            } else {
                // user@host:something — treat colon-suffix as part of host
                ParsedServer {
                    user: Some(user.to_string()),
                    host: rest.to_string(),
                    port: None,
                    path: None,
                }
            }
        } else {
            // user@host
            ParsedServer {
                user: Some(user.to_string()),
                host: rest.to_string(),
                port: None,
                path: None,
            }
        }
    } else if let Some((host, port_str)) = server.rsplit_once(':') {
        if let Ok(p) = port_str.parse::<u16>() {
            // host:port
            ParsedServer {
                host: host.to_string(),
                port: Some(p),
                ..Default::default()
            }
        } else {
            // host:something — treat as bare alias
            ParsedServer {
                host: server.to_string(),
                ..Default::default()
            }
        }
    } else {
        // bare alias or hostname
        ParsedServer {
            host: server.to_string(),
            ..Default::default()
        }
    }
}

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
    #[error(
        "protocol version mismatch: client={client}, server={server} — update kmuxd or kmux to the same version"
    )]
    VersionMismatch { client: u32, server: u32 },
}

/// Parse `server` into a `RemoteTarget`.
///
/// Returns `Some` when the string looks like a remote target (contains `@`,
/// or matches a hosts.toml alias that has a `user` configured).
/// Returns `None` for bare `host:port` strings (legacy direct-QUIC mode).
pub fn parse_remote_target(server: &str) -> Option<RemoteTarget> {
    let parsed = parse_server_string(server);
    resolve_remote_target(&parsed)
}

/// Resolve a `ParsedServer` into a `RemoteTarget` for SSH mode.
///
/// Returns `Some` when the parsed server looks like an SSH target (has a user,
/// or matches a hosts.toml alias with a user). Returns `None` for direct-QUIC.
pub fn resolve_remote_target(parsed: &ParsedServer) -> Option<RemoteTarget> {
    if parsed.user.is_some() {
        // Explicit user — apply hosts.toml overrides.
        let config = HostsConfig::load();
        let entry = config.get(&parsed.host).cloned().unwrap_or_default();
        return Some(RemoteTarget {
            user: parsed.user.clone(),
            host: entry.hostname.unwrap_or_else(|| parsed.host.clone()),
            ssh_port: parsed.port.or(entry.ssh_port),
        });
    }

    // No user in string — check hosts.toml alias.
    let config = HostsConfig::load();
    if let Some(entry) = config.get(&parsed.host)
        && entry.user.is_some()
    {
        return Some(RemoteTarget {
            user: entry.user.clone(),
            host: entry
                .hostname
                .clone()
                .unwrap_or_else(|| parsed.host.clone()),
            ssh_port: parsed.port.or(entry.ssh_port),
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_server_string tests ────────────────────────────────────────────

    #[test]
    fn parse_user_at_host() {
        assert_eq!(
            parse_server_string("alice@example.com"),
            ParsedServer {
                user: Some("alice".into()),
                host: "example.com".into(),
                port: None,
                path: None,
            }
        );
    }

    #[test]
    fn parse_user_at_host_with_path() {
        assert_eq!(
            parse_server_string("alice@example.com:/home/alice/project"),
            ParsedServer {
                user: Some("alice".into()),
                host: "example.com".into(),
                port: None,
                path: Some("/home/alice/project".into()),
            }
        );
    }

    #[test]
    fn parse_user_at_host_with_port() {
        assert_eq!(
            parse_server_string("alice@example.com:2222"),
            ParsedServer {
                user: Some("alice".into()),
                host: "example.com".into(),
                port: Some(2222),
                path: None,
            }
        );
    }

    #[test]
    fn parse_host_colon_port() {
        assert_eq!(
            parse_server_string("192.168.1.1:7777"),
            ParsedServer {
                host: "192.168.1.1".into(),
                port: Some(7777),
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_bare_alias() {
        assert_eq!(
            parse_server_string("devbox"),
            ParsedServer {
                host: "devbox".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_root_path() {
        assert_eq!(
            parse_server_string("bob@server:/"),
            ParsedServer {
                user: Some("bob".into()),
                host: "server".into(),
                port: None,
                path: Some("/".into()),
            }
        );
    }

    #[test]
    fn parse_path_with_spaces_and_special_chars() {
        assert_eq!(
            parse_server_string("bob@server:/home/bob/my project"),
            ParsedServer {
                user: Some("bob".into()),
                host: "server".into(),
                port: None,
                path: Some("/home/bob/my project".into()),
            }
        );
    }

    // ── parse_remote_target tests ────────────────────────────────────────────

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
    fn parse_remote_target_user_at_host_with_path() {
        // user@host:/path should still resolve to an SSH target
        let t = parse_remote_target("alice@example.com:/home/alice").unwrap();
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "example.com");
    }

    #[test]
    fn parse_remote_target_user_at_host_with_port() {
        let t = parse_remote_target("alice@example.com:2222").unwrap();
        assert_eq!(t.user.as_deref(), Some("alice"));
        assert_eq!(t.host, "example.com");
        assert_eq!(t.ssh_port, Some(2222));
    }
}
