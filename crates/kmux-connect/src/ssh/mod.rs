// `negotiate` runs `ssh ... kmuxd probe-or-start` and stands up an `-L` tunnel —
// the only remote-transport part of this module. The server-string parsing and
// the `RemoteTarget` / `SshError` types below stay ungated so a lean GUI build
// can still turn a `--server user@host` string into a `PeerTarget` for OpenPeer.
#[cfg(feature = "remote")]
mod negotiate;
#[cfg(feature = "remote")]
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
/// - `user@host:2222`   — SSH with explicit SSH port (`:` + digits = port)
/// - `user@host`        — SSH with default port
/// - `host:port`        — SSH on `port` with default user (`$USER`)
/// - `host`             — SSH with default user and port (or `hosts.toml` alias)
///
/// Every port appearing in a server string is the **SSH** port. Daemon
/// data-plane ports (QUIC, TCP+TLS) are ephemeral and exchanged in-band.
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
    /// Raw JSON response from `kmuxd probe-or-start`. Kept so diagnostic
    /// observers (e.g. `--dry-run`) can print the unredacted server reply.
    /// The field is present on every session; the token is redacted by the
    /// observer at print time, not in this value.
    pub probe_json: String,
}

/// Why a single `ssh ... kmuxd probe-or-start` invocation failed.
///
/// SSH's own non-zero exit codes are highly overloaded (255 covers auth,
/// network, and host-key failures all at once), so we only classify
/// the *layer* of the failure and rely on the captured stderr inside
/// [`SshError::ProbeFailed`] to tell the user what specifically broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailureKind {
    /// Exit 127: the remote shell could not exec `kmuxd`. The user needs
    /// to install kmuxd or fix their `PATH` on the remote.
    RemoteDaemonNotInstalled,
    /// Exit 255: ssh itself failed (auth, network, host-key, host down).
    /// Captured stderr disambiguates between these.
    SshFailed,
    /// Any other non-zero exit: kmuxd ran but probe-or-start failed.
    RemoteDaemonStartFailed,
}

impl std::fmt::Display for ProbeFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RemoteDaemonNotInstalled => f.write_str("kmuxd not found on remote host"),
            Self::SshFailed => f.write_str("SSH connection failed"),
            Self::RemoteDaemonStartFailed => {
                f.write_str("kmuxd probe-or-start failed on remote host")
            }
        }
    }
}

/// Errors from [`negotiate`]. Every variant carries enough context that
/// the user can act on it without enabling debug tracing — the captured
/// `argv`, `exit_code`, and `stderr` are formatted directly into the
/// `Display` representation, so a plain `eprintln!("{e}")` is enough.
#[derive(Debug, thiserror::Error)]
pub enum SshError {
    /// Could not invoke `ssh` at all (binary missing, fork failed).
    #[error("could not run `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// `ssh ... kmuxd probe-or-start` failed. `kind` says at which layer;
    /// `stderr` is what the user needs to see to fix it.
    #[error(
        "{kind}\n  ssh dest: {dest}\n  argv:     {argv}\n  exit:     {exit_code}\n  stderr:\n{stderr}"
    )]
    ProbeFailed {
        kind: ProbeFailureKind,
        dest: String,
        argv: String,
        exit_code: String,
        stderr: String,
    },

    /// `kmuxd probe-or-start` returned successfully but the JSON wasn't parseable.
    /// Either an old daemon predating the JSON contract, or a daemon crash that
    /// printed text to stdout.
    #[error("remote daemon returned malformed JSON: {error}\n  raw output:\n    {raw}")]
    BadProbeJson { error: String, raw: String },

    /// Protocol-range gate: client and remote daemon have no common schema.
    #[error(
        "protocol version mismatch: client={client}, server={server} \
         — update kmuxd or kmux until their supported ranges overlap"
    )]
    VersionMismatch { client: String, server: String },

    /// Could not pre-allocate a free local TCP port for the `-L` tunnel.
    #[error("could not allocate a local port for the SSH tunnel: {0}")]
    LocalPortAllocFailed(String),

    /// A host/user value would be ambiguous or unsafe to pass to OpenSSH.
    #[error("invalid SSH {field} {value:?}: {reason}")]
    InvalidTarget {
        field: &'static str,
        value: String,
        reason: &'static str,
    },

    /// `ssh -L -N` exited before the local forward came up.
    #[error(
        "SSH tunnel exited before becoming ready\n  ssh dest: {dest}\n  argv:     {argv}\n  exit:     {exit_code}\n  stderr:\n{stderr}"
    )]
    TunnelDiedEarly {
        dest: String,
        argv: String,
        exit_code: String,
        stderr: String,
    },

    /// We could not connect to the local end of the `-L` tunnel within the
    /// readiness timeout. The ssh process is still alive but isn't forwarding.
    #[error(
        "SSH tunnel never accepted a local connection\n  ssh dest:   {dest}\n  argv:       {argv}\n  local_port: {local_port}\n  stderr:\n{stderr}"
    )]
    TunnelUnreachable {
        dest: String,
        argv: String,
        local_port: u16,
        stderr: String,
    },
}

/// Parse `server` into a `RemoteTarget`.
///
/// Every non-empty server string resolves to an SSH target — there is no
/// direct-QUIC path. The user is taken from (in order): explicit `user@…`,
/// matching `hosts.toml` entry, `$USER` env var, or finally `None` (lets
/// the `ssh` CLI fall through to `~/.ssh/config` / OS default).
pub fn parse_remote_target(server: &str) -> Option<RemoteTarget> {
    let parsed = parse_server_string(server);
    resolve_remote_target(&parsed)
}

/// Resolve a `ParsedServer` into an SSH `RemoteTarget`.
///
/// Returns `Some` for every parsed server with a non-empty host. The user is
/// resolved from the explicit `user@` prefix, then `hosts.toml`, then `$USER`.
/// If none of those produces a value the field is left `None` and `ssh` will
/// fall through to its own defaults (`~/.ssh/config`, then OS user).
pub fn resolve_remote_target(parsed: &ParsedServer) -> Option<RemoteTarget> {
    let entry = HostsConfig::load()
        .get(&parsed.host)
        .cloned()
        .unwrap_or_default();
    resolve_with(parsed, entry, std::env::var("USER").ok())
}

/// [`resolve_remote_target`] with its inputs explicit: the `hosts.toml` entry
/// for the host (default if none) and the `$USER` value.
fn resolve_with(
    parsed: &ParsedServer,
    entry: HostEntry,
    env_user: Option<String>,
) -> Option<RemoteTarget> {
    if parsed.host.is_empty() {
        return None;
    }

    let user = parsed
        .user
        .clone()
        .or(entry.user)
        .or_else(|| env_user.filter(|s| !s.is_empty()));

    Some(RemoteTarget {
        user,
        host: entry.hostname.unwrap_or_else(|| parsed.host.clone()),
        ssh_port: parsed.port.or(entry.ssh_port),
    })
}

/// Apply per-host overrides from `hosts.toml` (`ssh_port`, user, hostname).
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

    #[test]
    fn parse_server_string_splits_every_form() {
        let parsed =
            |user: Option<&str>, host: &str, port: Option<u16>, path: Option<&str>| ParsedServer {
                user: user.map(Into::into),
                host: host.into(),
                port,
                path: path.map(Into::into),
            };
        let alice = Some("alice");
        let bob = Some("bob");
        let cases = [
            (
                "alice@example.com",
                parsed(alice, "example.com", None, None),
            ),
            (
                "alice@example.com:/home/alice/project",
                parsed(alice, "example.com", None, Some("/home/alice/project")),
            ),
            (
                "alice@example.com:2222",
                parsed(alice, "example.com", Some(2222), None),
            ),
            (
                "192.168.1.1:7777",
                parsed(None, "192.168.1.1", Some(7777), None),
            ),
            ("devbox", parsed(None, "devbox", None, None)),
            ("bob@server:/", parsed(bob, "server", None, Some("/"))),
            (
                "bob@server:/home/bob/my project",
                parsed(bob, "server", None, Some("/home/bob/my project")),
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_server_string(input), expected, "input {input:?}");
        }
    }

    /// The public entry points wire parsing to `resolve_with`. An explicit
    /// user and port win over this user's `hosts.toml` and `$USER`, so the
    /// result does not depend on either.
    #[test]
    fn parse_remote_target_resolves_an_explicit_user_host_and_port() {
        let target = parse_remote_target("someone@kmux-test.invalid:2222").expect("a target");
        assert_eq!(
            (
                target.user.as_deref(),
                target.host.as_str(),
                target.ssh_port
            ),
            (Some("someone"), "kmux-test.invalid", Some(2222))
        );
    }

    /// The user comes from `user@`, then the `hosts.toml` entry, then `$USER`;
    /// a port in the server string is always the SSH port.
    #[test]
    fn resolve_with_takes_user_host_and_port_in_precedence_order() {
        let alias = HostEntry {
            hostname: Some("prod.example.com".into()),
            user: Some("deploy".into()),
            ssh_port: Some(2200),
        };
        let none = HostEntry::default;
        let cases = [
            (
                "alice@example.com",
                none(),
                (Some("alice"), "example.com", None),
            ),
            (
                "alice@example.com:/home/alice",
                none(),
                (Some("alice"), "example.com", None),
            ),
            (
                "alice@example.com:2222",
                none(),
                (Some("alice"), "example.com", Some(2222)),
            ),
            (
                "192.168.1.1:7777",
                none(),
                (Some("me"), "192.168.1.1", Some(7777)),
            ),
            ("focalors", none(), (Some("me"), "focalors", None)),
            (
                "prod",
                alias.clone(),
                (Some("deploy"), "prod.example.com", Some(2200)),
            ),
            (
                "alice@prod:22",
                alias,
                (Some("alice"), "prod.example.com", Some(22)),
            ),
        ];
        for (input, entry, (user, host, ssh_port)) in cases {
            let t = resolve_with(&parse_server_string(input), entry, Some("me".into()))
                .unwrap_or_else(|| panic!("{input:?} resolves"));
            assert_eq!(
                (t.user.as_deref(), t.host.as_str(), t.ssh_port),
                (user, host, ssh_port),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn resolve_with_leaves_the_user_to_ssh_without_user_or_env() {
        for env_user in [None, Some(String::new())] {
            let t = resolve_with(
                &parse_server_string("focalors"),
                HostEntry::default(),
                env_user,
            )
            .unwrap();
            assert_eq!(t.user, None);
        }
        assert!(resolve_with(&ParsedServer::default(), HostEntry::default(), None).is_none());
    }
}
