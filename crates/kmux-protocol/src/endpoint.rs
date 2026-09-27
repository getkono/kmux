/// Parsed endpoint for connecting to kmuxd.
///
/// Supported URL grammar:
///
/// | Pattern                        | Parsed as                            |
/// |--------------------------------|--------------------------------------|
/// | `quic://host:port`             | `Quic { host, port }`                |
/// | `tcp+tls://host:port`          | `TcpTls { host, port }`              |
/// | `unix:///absolute/path`        | `Unix(path)`                         |
/// | `ssh://[user@]host[:port]`     | `Ssh { user, host, ssh_port }`       |
/// | `user@host[:port]`             | `Ssh { user: Some, host, ssh_port }` |
/// | `host:port`                    | `Quic { host, port }`  (sugar)       |
/// | `@alias`                       | `Alias(name)` — hosts.toml lookup    |
///
/// See `docs/connection.md` for the full connection model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Quic {
        host: String,
        port: u16,
    },
    TcpTls {
        host: String,
        port: u16,
    },
    Unix(std::path::PathBuf),
    Ssh {
        user: Option<String>,
        host: String,
        ssh_port: Option<u16>,
    },
    /// Alias key — must be resolved via `hosts.toml` before use.
    Alias(String),
}

/// Error returned when an endpoint string cannot be parsed.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseEndpointError(pub String);

impl std::fmt::Display for ParseEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid endpoint: {}", self.0)
    }
}

impl std::error::Error for ParseEndpointError {}

impl Endpoint {
    /// Parse an endpoint string according to the grammar above.
    pub fn parse(s: &str) -> Result<Self, ParseEndpointError> {
        let s = s.trim();

        // ── @alias ─────────────────────────────────────────────────────────────
        if let Some(alias) = s.strip_prefix('@') {
            if alias.is_empty() {
                return Err(ParseEndpointError("empty alias name after '@'".into()));
            }
            return Ok(Self::Alias(alias.to_string()));
        }

        // ── quic:// ────────────────────────────────────────────────────────────
        if let Some(rest) = s.strip_prefix("quic://") {
            let (host, port) = split_host_port(rest).ok_or_else(|| {
                ParseEndpointError(format!("quic:// endpoint requires host:port, got '{rest}'"))
            })?;
            return Ok(Self::Quic { host, port });
        }

        // ── tcp+tls:// ────────────────────────────────────────────────────────
        if let Some(rest) = s.strip_prefix("tcp+tls://") {
            let (host, port) = split_host_port(rest).ok_or_else(|| {
                ParseEndpointError(format!(
                    "tcp+tls:// endpoint requires host:port, got '{rest}'"
                ))
            })?;
            return Ok(Self::TcpTls { host, port });
        }

        // ── unix:// ────────────────────────────────────────────────────────────
        if let Some(path_str) = s.strip_prefix("unix://") {
            if path_str.is_empty() {
                return Err(ParseEndpointError("unix:// requires a path".into()));
            }
            // unix:///absolute/path  →  path_str starts with "/"
            // unix://relative        →  also accepted but unusual
            return Ok(Self::Unix(std::path::PathBuf::from(path_str)));
        }

        // ── ssh:// ────────────────────────────────────────────────────────────
        if let Some(rest) = s.strip_prefix("ssh://") {
            return parse_ssh_authority(rest);
        }

        // ── user@host[:port] ─────────────────────────────────────────────────
        if let Some(at_pos) = s.find('@') {
            let user = &s[..at_pos];
            let host_part = &s[at_pos + 1..];
            if !user.is_empty() {
                let (host, ssh_port) = split_optional_port(host_part);
                return Ok(Self::Ssh {
                    user: Some(user.to_string()),
                    host,
                    ssh_port,
                });
            }
        }

        // ── host:port  (QUIC sugar) ────────────────────────────────────────────
        if let Some((host, port)) = split_host_port(s) {
            return Ok(Self::Quic { host, port });
        }

        // ── bare hostname/alias (not @-prefixed) ─────────────────────────────
        // Treat as an alias to be resolved from hosts.toml.
        if !s.is_empty() {
            return Ok(Self::Alias(s.to_string()));
        }

        Err(ParseEndpointError("empty endpoint string".into()))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Quic { host, port } => write!(f, "quic://{host}:{port}"),
            Self::TcpTls { host, port } => write!(f, "tcp+tls://{host}:{port}"),
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::Ssh {
                user: Some(u),
                host,
                ssh_port: Some(p),
            } => write!(f, "ssh://{u}@{host}:{p}"),
            Self::Ssh {
                user: Some(u),
                host,
                ssh_port: None,
            } => write!(f, "ssh://{u}@{host}"),
            Self::Ssh {
                user: None,
                host,
                ssh_port: Some(p),
            } => write!(f, "ssh://{host}:{p}"),
            Self::Ssh {
                user: None,
                host,
                ssh_port: None,
            } => write!(f, "ssh://{host}"),
            Self::Alias(name) => write!(f, "@{name}"),
        }
    }
}

/// Parse `user@host[:port]` or `host[:port]` from an SSH authority string.
fn parse_ssh_authority(authority: &str) -> Result<Endpoint, ParseEndpointError> {
    if let Some(at_pos) = authority.find('@') {
        let user = &authority[..at_pos];
        let host_part = &authority[at_pos + 1..];
        let (host, ssh_port) = split_optional_port(host_part);
        Ok(Endpoint::Ssh {
            user: if user.is_empty() {
                None
            } else {
                Some(user.to_string())
            },
            host,
            ssh_port,
        })
    } else {
        let (host, ssh_port) = split_optional_port(authority);
        Ok(Endpoint::Ssh {
            user: None,
            host,
            ssh_port,
        })
    }
}

/// Split `host:port` — returns `None` if port is missing or non-numeric.
///
/// For IPv6 literal addresses like `[::1]:8443`, the brackets are preserved in
/// the host portion so callers can normalise them if needed.
fn split_host_port(s: &str) -> Option<(String, u16)> {
    // Handle IPv6: "[::1]:port"
    // `bracket_end` is the index of `]` within the stripped string (s without `[`),
    // so in `s`: `[` is at 0, host contents are at 1..bracket_end+1, `]` is at
    // bracket_end+1, `:` is at bracket_end+2, port starts at bracket_end+3.
    if let Some(bracket_end) = s.strip_prefix('[').and_then(|r| r.find(']')) {
        let host = format!("[{}]", &s[1..=bracket_end]);
        let after = &s[bracket_end + 3..]; // skip `]:` (]  is at bracket_end+1, : at bracket_end+2)
        let port: u16 = after.parse().ok()?;
        return Some((host, port));
    }

    // Plain host:port — use the LAST ':' to support hostnames that happen to
    // contain colons only in IPv6 bare form (not common, but defensive).
    let colon = s.rfind(':')?;
    let host = s[..colon].to_string();
    let port: u16 = s[colon + 1..].parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// Split optional port from a host-or-host:port string.
/// Returns `(host, Some(port))` or `(host, None)`.
fn split_optional_port(s: &str) -> (String, Option<u16>) {
    if let Some((host, port_str)) = s.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return (host.to_string(), Some(port));
    }
    (s.to_string(), None)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn quic(host: &str, port: u16) -> Endpoint {
        Endpoint::Quic {
            host: host.into(),
            port,
        }
    }

    fn ssh(user: Option<&str>, host: &str, ssh_port: Option<u16>) -> Endpoint {
        Endpoint::Ssh {
            user: user.map(Into::into),
            host: host.into(),
            ssh_port,
        }
    }

    #[test]
    fn parse_accepts_every_grammar_form() {
        let cases = [
            ("quic://host.example:8443", quic("host.example", 8443)),
            ("quic://[::1]:8443", quic("[::1]", 8443)),
            (
                "tcp+tls://prod.example.com:8444",
                Endpoint::TcpTls {
                    host: "prod.example.com".into(),
                    port: 8444,
                },
            ),
            (
                "unix:///run/user/1000/kmux/daemon-data.sock",
                Endpoint::Unix(PathBuf::from("/run/user/1000/kmux/daemon-data.sock")),
            ),
            (
                "ssh://alice@example.com:2222",
                ssh(Some("alice"), "example.com", Some(2222)),
            ),
            (
                "ssh://alice@example.com",
                ssh(Some("alice"), "example.com", None),
            ),
            ("ssh://example.com", ssh(None, "example.com", None)),
            // user@host sugar
            (
                "alice@host.example",
                ssh(Some("alice"), "host.example", None),
            ),
            (
                "alice@host.example:2222",
                ssh(Some("alice"), "host.example", Some(2222)),
            ),
            // host:port is QUIC sugar
            ("myserver.local:9000", quic("myserver.local", 9000)),
            ("@prod", Endpoint::Alias("prod".into())),
            // A bare name with no ':' and no '@' is treated as an alias.
            ("devbox", Endpoint::Alias("devbox".into())),
        ];
        for (input, expected) in cases {
            assert_eq!(Endpoint::parse(input), Ok(expected), "input {input:?}");
        }
    }

    #[test]
    fn display_is_canonical_and_parses_back() {
        let cases = [
            (quic("host.example", 8443), "quic://host.example:8443"),
            (
                Endpoint::TcpTls {
                    host: "host.example".into(),
                    port: 8444,
                },
                "tcp+tls://host.example:8444",
            ),
            (
                Endpoint::Unix(PathBuf::from("/run/user/1000/kmux/daemon-data.sock")),
                "unix:///run/user/1000/kmux/daemon-data.sock",
            ),
            (
                ssh(Some("alice"), "example.com", Some(2222)),
                "ssh://alice@example.com:2222",
            ),
            (Endpoint::Alias("prod".into()), "@prod"),
        ];
        for (ep, expected) in cases {
            let shown = ep.to_string();
            assert_eq!(shown, expected, "display of {ep:?}");
            assert_eq!(Endpoint::parse(&shown), Ok(ep), "parse back {shown:?}");
        }
    }

    #[test]
    fn parse_rejects_malformed_input() {
        for input in ["", "quic://host.example", "@"] {
            assert!(Endpoint::parse(input).is_err(), "input {input:?}");
        }
    }
}
