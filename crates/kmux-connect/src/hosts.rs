use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Global SSH defaults applied to all remote connections.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SshDefaults {
    /// Path to the ssh binary (default: "ssh").
    pub binary: Option<String>,
    /// SSH connection timeout in seconds.
    pub connect_timeout: Option<u64>,
}

/// Per-host connection configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HostEntry {
    /// Actual hostname to connect to (overrides the alias key).
    pub hostname: Option<String>,
    /// SSH user (e.g. "deploy").
    pub user: Option<String>,
    /// Non-standard SSH port.
    pub ssh_port: Option<u16>,
}

/// Contents of `~/.config/kmux/hosts.toml`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HostsConfig {
    pub ssh: Option<SshDefaults>,
    #[serde(default)]
    pub hosts: HashMap<String, HostEntry>,
}

#[derive(Debug, Error)]
pub enum HostsConfigError {
    #[error("failed to read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

impl HostsConfig {
    /// Load from the default path (`~/.config/kmux/hosts.toml`).
    /// Returns an empty config if the file doesn't exist or can't be parsed.
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    /// Load from the default path and surface errors for existing malformed
    /// files. A missing file is not an error.
    pub fn try_load() -> Result<Self, HostsConfigError> {
        let path = hosts_config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                toml::from_str(&contents).map_err(|source| HostsConfigError::Parse { path, source })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(HostsConfigError::Read { path, source }),
        }
    }

    /// Look up a host alias and return the resolved `HostEntry`.
    pub fn get(&self, alias: &str) -> Option<&HostEntry> {
        self.hosts.get(alias)
    }
}

/// Path to the hosts config file (`~/.config/kmux/hosts.toml`).
pub fn hosts_config_path() -> PathBuf {
    // Reuse the same XDG_CONFIG_HOME resolution as the rest of the codebase.
    if let Ok(base) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(base)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config")
    } else {
        PathBuf::from(".config")
    }
    .join("kmux")
    .join("hosts.toml")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshHostSource {
    KmuxHostsToml,
    OpenSshConfig(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSshHost {
    pub alias: String,
    pub user: Option<String>,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub source: SshHostSource,
}

impl DiscoveredSshHost {
    pub fn display_label(&self) -> String {
        match (&self.source, &self.user) {
            (SshHostSource::KmuxHostsToml, Some(user)) => {
                format!("{user}@{}", self.alias)
            }
            _ => self.alias.clone(),
        }
    }
}

/// Discover SSH aliases configured by kmux and OpenSSH.
///
/// OpenSSH entries intentionally carry only their alias. The `ssh` executable
/// remains authoritative for effective option precedence when kmux connects.
pub fn discover_ssh_hosts() -> Vec<DiscoveredSshHost> {
    discover_ssh_hosts_from_paths(default_ssh_config_paths())
}

fn discover_ssh_hosts_from_paths(
    paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<DiscoveredSshHost> {
    discover_ssh_hosts_with_config(HostsConfig::try_load(), paths)
}

fn discover_ssh_hosts_with_config(
    config: Result<HostsConfig, HostsConfigError>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> Vec<DiscoveredSshHost> {
    let mut hosts = Vec::new();
    let config = config.unwrap_or_else(|error| {
        tracing::warn!(%error, "ignoring invalid kmux hosts configuration");
        HostsConfig::default()
    });
    for (alias, entry) in config.hosts {
        hosts.push(DiscoveredSshHost {
            alias,
            user: entry.user,
            hostname: entry.hostname,
            port: entry.ssh_port,
            source: SshHostSource::KmuxHostsToml,
        });
    }

    let mut visited = HashSet::new();
    for path in paths {
        parse_openssh_config(&path, &mut visited, &mut hosts);
    }
    dedupe_hosts(hosts)
}

fn dedupe_hosts(hosts: Vec<DiscoveredSshHost>) -> Vec<DiscoveredSshHost> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for host in hosts {
        if seen.insert(host.alias.clone()) {
            out.push(host);
        }
    }
    out.sort_by(|a, b| a.alias.cmp(&b.alias));
    out
}

fn default_ssh_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = home_dir() {
        paths.push(home.join(".ssh").join("config"));
    }
    paths.push(PathBuf::from("/etc/ssh/ssh_config"));
    paths
}

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

fn parse_openssh_config(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    hosts: &mut Vec<DiscoveredSshHost>,
) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        children.sort();
        for child in children {
            parse_openssh_config(&child, visited, hosts);
        }
        return;
    }
    if !path.is_file() {
        return;
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return;
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    let source = SshHostSource::OpenSshConfig(path.to_path_buf());

    for line in raw.lines() {
        let mut parts = tokenize_openssh_line(line);
        if parts.is_empty() {
            continue;
        }
        split_keyword_equals(&mut parts);
        let key = parts.remove(0);
        if key.eq_ignore_ascii_case("Host") {
            hosts.extend(
                parts
                    .into_iter()
                    .filter(|value| explicit_host_alias(value))
                    .map(|alias| DiscoveredSshHost {
                        alias,
                        user: None,
                        hostname: None,
                        port: None,
                        source: source.clone(),
                    }),
            );
        } else if key.eq_ignore_ascii_case("Include") {
            for value in parts {
                for include in expand_include(path, &value) {
                    parse_openssh_config(&include, visited, hosts);
                }
            }
        }
    }
}

fn tokenize_openssh_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in line.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '#' => break,
            ch if ch.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn split_keyword_equals(parts: &mut Vec<String>) {
    let Some((key, value)) = parts
        .first()
        .and_then(|first| first.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
    else {
        return;
    };
    parts[0] = key;
    if !value.is_empty() {
        parts.insert(1, value);
    }
}

fn explicit_host_alias(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('!')
        && !value.contains('*')
        && !value.contains('?')
        && value != "*"
}

fn expand_include(current_file: &Path, value: &str) -> Vec<PathBuf> {
    let path = if value.starts_with("~/") {
        match home_dir() {
            Some(home) => home.join(value.trim_start_matches("~/")),
            None => return Vec::new(),
        }
    } else {
        let raw = PathBuf::from(value);
        if raw.is_absolute() {
            raw
        } else {
            current_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(raw)
        }
    };
    if !value.contains('*') && !value.contains('?') {
        return vec![path];
    }
    expand_simple_glob(&path)
}

fn expand_simple_glob(pattern: &Path) -> Vec<PathBuf> {
    let Some(parent) = pattern.parent() else {
        return Vec::new();
    };
    let Some(file_pat) = pattern.file_name().and_then(|f| f.to_str()) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|path| {
            path.file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|name| glob_match(file_pat, name))
        })
        .collect();
    out.sort();
    out
}

fn glob_match(pattern: &str, value: &str) -> bool {
    fn inner(pat: &[u8], val: &[u8]) -> bool {
        match (pat.first(), val.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(b'*'), _) => inner(&pat[1..], val) || (!val.is_empty() && inner(pat, &val[1..])),
            (Some(b'?'), Some(_)) => inner(&pat[1..], &val[1..]),
            (Some(p), Some(v)) if p == v => inner(&pat[1..], &val[1..]),
            _ => false,
        }
    }
    inner(pattern.as_bytes(), value.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_config_deserializes_full() {
        let toml = r#"
[ssh]
connect_timeout = 10

[hosts.devbox]
hostname = "dev.example.com"
user = "deploy"
ssh_port = 2222

[hosts.prod]
hostname = "prod.internal"
user = "admin"
"#;
        let config: HostsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.ssh.as_ref().unwrap().connect_timeout, Some(10));

        let devbox = config.get("devbox").unwrap();
        assert_eq!(devbox.hostname.as_deref(), Some("dev.example.com"));
        assert_eq!(devbox.user.as_deref(), Some("deploy"));
        assert_eq!(devbox.ssh_port, Some(2222));

        let prod = config.get("prod").unwrap();
        assert_eq!(prod.hostname.as_deref(), Some("prod.internal"));
        assert_eq!(prod.user.as_deref(), Some("admin"));
        assert!(prod.ssh_port.is_none());
    }

    #[test]
    fn hosts_config_empty_is_default() {
        let config: HostsConfig = toml::from_str("").unwrap();
        assert!(config.ssh.is_none());
        assert!(config.hosts.is_empty());
        assert!(config.get("nonexistent").is_none());
    }

    #[test]
    fn openssh_config_discovers_explicit_hosts_and_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let include_dir = tmp.path().join("config.d");
        std::fs::create_dir(&include_dir).unwrap();
        let include = include_dir.join("work.conf");
        std::fs::write(
            &include,
            "Host=prod *.wild !blocked\n  HostName prod.internal\n  User deploy\n  Port 2200\n",
        )
        .unwrap();
        let root = tmp.path().join("config");
        std::fs::write(
            &root,
            format!(
                "Include {}\nHost dev box?\n  HostName dev.internal\n  User alice\n",
                include.display()
            ),
        )
        .unwrap();

        let mut visited = HashSet::new();
        let mut hosts = Vec::new();
        parse_openssh_config(&root, &mut visited, &mut hosts);
        hosts.sort_by(|a, b| a.alias.cmp(&b.alias));

        assert_eq!(
            hosts.iter().map(|h| h.alias.as_str()).collect::<Vec<_>>(),
            vec!["dev", "prod"]
        );
        let prod = hosts.iter().find(|h| h.alias == "prod").unwrap();
        assert_eq!(prod.user, None);
        assert_eq!(prod.hostname, None);
        assert_eq!(prod.port, None);
        assert_eq!(prod.display_label(), "prod");
        let dev = hosts.iter().find(|h| h.alias == "dev").unwrap();
        assert_eq!(dev.user, None);
        assert_eq!(dev.hostname, None);
    }

    #[test]
    fn discovery_reads_only_roots_and_explicit_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("config");
        let unreferenced = tmp.path().join("config.d");
        std::fs::create_dir(&unreferenced).unwrap();
        std::fs::write(&root, "Host root\n").unwrap();
        std::fs::write(unreferenced.join("backup.conf"), "Host backup\n").unwrap();

        let hosts = discover_ssh_hosts_with_config(Ok(HostsConfig::default()), [root]);
        assert_eq!(
            hosts
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
    }

    #[test]
    fn malformed_kmux_config_does_not_suppress_openssh_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("config");
        std::fs::write(&root, "Host survives\n").unwrap();
        let error = HostsConfigError::Read {
            path: tmp.path().join("hosts.toml"),
            source: std::io::Error::other("broken"),
        };

        let hosts = discover_ssh_hosts_with_config(Err(error), [root]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "survives");
    }
}
