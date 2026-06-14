use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

impl HostsConfig {
    /// Load from the default path (`~/.config/kmux/hosts.toml`).
    /// Returns an empty config if the file doesn't exist or can't be parsed.
    pub fn load() -> Self {
        let path = hosts_config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
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
}
