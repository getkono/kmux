//! `kmuxd.toml` configuration: parse, validate, and supply defaults.
//!
//! Phase 7: provides a structured `ServerConfig` used by `startup.rs` and
//! the announcement model in `announce.rs`.
//!
//! Config file search order (first found wins):
//!
//! 1. Path from `--config <path>`
//! 2. `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml`
//! 3. `/etc/kmuxd/kmuxd.toml`
//!
//! Missing file → built-in defaults (QUIC on `[::]:0`, TLS-TCP on `[::]:0`, UDS auto).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ─── Schema ──────────────────────────────────────────────────────────────────

/// Top-level `kmuxd.toml` structure.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Config schema version (optional; 1 is current).
    #[serde(default = "default_config_version")]
    pub version: u32,

    /// Global settings.
    #[serde(default)]
    pub log_level: Option<String>,

    /// Where the daemon writes runtime files (UDS socket, PID, token).
    /// `"auto"` resolves to `$XDG_RUNTIME_DIR/kmux` or `/tmp/kmux-<uid>`.
    #[serde(default = "default_runtime_dir")]
    pub runtime_dir: String,

    /// TLS certificate configuration (required for QUIC and TCP+TLS listeners).
    #[serde(default)]
    pub tls: TlsConfig,

    /// Transport listeners. One block per protocol.
    #[serde(default = "default_listen")]
    pub listen: Vec<ListenConfig>,

    /// Advertise overrides — controls what the server announces to clients.
    #[serde(default)]
    pub advertise: AdvertiseConfig,

    /// Auth configuration.
    #[serde(default)]
    pub auth: AuthConfig,

    /// Daemon lifecycle settings.
    #[serde(default)]
    pub daemon: DaemonConfig,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            version: 1,
            log_level: None,
            runtime_dir: default_runtime_dir(),
            tls: TlsConfig::default(),
            listen: default_listen(),
            advertise: AdvertiseConfig::default(),
            auth: AuthConfig::default(),
            daemon: DaemonConfig::default(),
        }
    }
}

/// Daemon lifecycle settings (`[daemon]` in `kmuxd.toml`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Exit when no clients have been connected for this many seconds.
    /// `0` disables idle shutdown (daemon runs until explicitly stopped).
    ///
    /// Note: the debounce applies per-transport-disconnect. A 30 s window
    /// is long enough for clients to reconnect across transport switches.
    #[serde(default = "default_idle_shutdown_secs")]
    pub idle_shutdown_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_shutdown_secs: default_idle_shutdown_secs(),
        }
    }
}

fn default_idle_shutdown_secs() -> u64 {
    30
}

fn default_config_version() -> u32 {
    1
}
fn default_runtime_dir() -> String {
    "auto".to_string()
}

/// TLS certificate source.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Path to the PEM certificate file.
    pub cert: Option<String>,
    /// Path to the PEM private key file.
    pub key: Option<String>,
    /// Generate an in-memory self-signed certificate (development only).
    #[serde(default)]
    pub self_signed: bool,
}

/// A single transport listener block (`[[listen]]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    /// Transport kind: `"quic"`, `"tcp+tls"`, or `"unix"`.
    pub kind: ListenKind,

    /// Bind address (ignored for `kind = "unix"`).
    #[serde(default = "default_bind")]
    pub bind: String,

    /// Port to listen on (ignored for `kind = "unix"`).
    /// Always `0` (ephemeral) unless overridden by the `--port` / `--tcp-port`
    /// CLI flags. Not configurable via `kmuxd.toml` — ports are assigned by the
    /// kernel and announced to clients so that they never need a fixed value.
    #[serde(skip, default)]
    pub port: u16,

    /// Whether this listener is enabled (admin can disable without removing the block).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Socket path for `kind = "unix"`. `"auto"` resolves to the runtime dir path.
    #[serde(default = "default_auto")]
    pub path: String,

    /// Audience restriction: controls who this endpoint is announced to.
    #[serde(default)]
    pub audience: Audience,

    /// Admin-controlled priority bias (higher = more preferred by scorer).
    #[serde(default)]
    pub priority: i32,
}

fn default_bind() -> String {
    // Bind to all IPv4 interfaces by default. We previously used "::" for
    // IPv6 dual-stack, but on hosts where `net.ipv6.bindv6only=1` (some
    // distro defaults) the listener silently drops IPv4 traffic — making
    // QUIC unreachable from typical IPv4 networks (Tailscale, LANs).
    // `0.0.0.0` is unambiguously usable from every IPv4-reachable client.
    // Operators who need IPv6 can set `bind = "::"` explicitly.
    "0.0.0.0".to_string()
}
fn default_true() -> bool {
    true
}
fn default_auto() -> String {
    "auto".to_string()
}

fn default_listen() -> Vec<ListenConfig> {
    vec![
        ListenConfig {
            kind: ListenKind::Quic,
            bind: "::".to_string(),
            port: 0,
            enabled: true,
            path: "auto".to_string(),
            audience: Audience::Any,
            priority: 0,
        },
        ListenConfig {
            kind: ListenKind::TcpTls,
            bind: "::".to_string(),
            port: 0,
            enabled: true,
            path: "auto".to_string(),
            audience: Audience::Any,
            priority: 0,
        },
        ListenConfig {
            kind: ListenKind::Unix,
            bind: "::".to_string(),
            port: 0,
            enabled: true,
            path: "auto".to_string(),
            audience: Audience::Local,
            priority: 0,
        },
    ]
}

/// Transport protocol for a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListenKind {
    Quic,
    #[serde(rename = "tcp+tls")]
    TcpTls,
    Unix,
}

/// Who this endpoint is announced to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Audience {
    /// Announced to all clients (default).
    #[default]
    Any,
    /// Announced only to clients on RFC-1918 / link-local addresses.
    Lan,
    /// Announced only when the client bootstrapped via the local UDS control socket
    /// or SSH from `127.0.0.1`.
    Local,
    /// Announced only inside SSH `probe-or-start` responses.
    SshOnly,
}

/// Advertise overrides — controls what the server tells clients about itself.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertiseConfig {
    /// Public hostname substituted into advertised addresses for non-local clients.
    /// When `None`, the bind address is used as-is.
    pub public_host: Option<String>,
}

/// Auth configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Path to the token file. `"auto"` uses the runtime dir.
    #[serde(default = "default_auto")]
    pub token_file: String,

    /// Accept `SO_PEERCRED` peer-uid match in lieu of token on UDS connections.
    #[serde(default = "default_true")]
    pub allow_peer_cred: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token_file: default_auto(),
            allow_peer_cred: true,
        }
    }
}

// ─── Effective server config ─────────────────────────────────────────────────

/// Resolved, validated server configuration (computed from `ConfigFile` + CLI overrides).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub tls: TlsConfig,
    pub listeners: Vec<ListenConfig>,
    pub advertise: AdvertiseConfig,
    pub auth: AuthConfig,
    pub runtime_dir: String,
    /// Seconds of inactivity (zero clients) before the daemon exits. `0` = disabled.
    pub idle_shutdown_secs: u64,
}

impl ServerConfig {
    /// Validate the config and return a resolved `ServerConfig`.
    pub fn resolve(file: ConfigFile) -> anyhow::Result<Self> {
        // At least one listener must be enabled.
        if !file.listen.iter().any(|l| l.enabled) {
            anyhow::bail!("no enabled listeners in configuration");
        }

        // TLS is required when any QUIC or TCP+TLS listener is enabled.
        let needs_tls = file
            .listen
            .iter()
            .any(|l| l.enabled && matches!(l.kind, ListenKind::Quic | ListenKind::TcpTls));
        if needs_tls && !file.tls.self_signed && file.tls.cert.is_none() {
            anyhow::bail!(
                "TLS cert+key or self_signed = true is required when QUIC or TCP+TLS listeners are enabled"
            );
        }

        Ok(Self {
            tls: file.tls,
            listeners: file.listen,
            advertise: file.advertise,
            auth: file.auth,
            runtime_dir: file.runtime_dir,
            idle_shutdown_secs: file.daemon.idle_shutdown_secs,
        })
    }
}

// ─── Loading ─────────────────────────────────────────────────────────────────

/// Load config from an explicit path, or discover from standard locations.
///
/// Returns `(config, source_path)` where `source_path` is `None` when built-in
/// defaults are used.
pub fn load_config(explicit_path: Option<&Path>) -> anyhow::Result<(ConfigFile, Option<PathBuf>)> {
    if let Some(path) = explicit_path {
        let content = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("cannot read config {}: {e}", path.display()))?;
        let cfg: ConfigFile = toml::from_str(&content)
            .map_err(|e| anyhow::anyhow!("cannot parse config {}: {e}", path.display()))?;
        return Ok((cfg, Some(path.to_path_buf())));
    }

    // Search standard paths.
    for candidate in search_paths() {
        if candidate.exists() {
            let content = std::fs::read_to_string(&candidate)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", candidate.display()))?;
            let cfg: ConfigFile = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("cannot parse {}: {e}", candidate.display()))?;
            return Ok((cfg, Some(candidate)));
        }
    }

    // No config file found — use defaults.
    Ok((ConfigFile::default(), None))
}

fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // $XDG_CONFIG_HOME/kmuxd/kmuxd.toml
    if let Ok(base) = std::env::var("XDG_CONFIG_HOME") {
        paths.push(PathBuf::from(base).join("kmuxd").join("kmuxd.toml"));
    } else if let Ok(home) = std::env::var("HOME") {
        paths.push(
            PathBuf::from(home)
                .join(".config")
                .join("kmuxd")
                .join("kmuxd.toml"),
        );
    }

    paths.push(PathBuf::from("/etc/kmuxd/kmuxd.toml"));
    paths
}

/// Write the default config to a path (used on first run to create a template).
pub fn write_default_config(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cfg = ConfigFile::default();
    let toml_str = toml::to_string_pretty(&cfg)?;
    std::fs::write(path, toml_str)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_CONFIG: &str = r#"
version = 1
runtime_dir = "/var/run/kmux"

[tls]
cert = "/etc/kmuxd/cert.pem"
key  = "/etc/kmuxd/key.pem"

[[listen]]
kind = "quic"
bind = "::"
enabled = true
audience = "any"
priority = 0

[[listen]]
kind = "tcp+tls"
bind = "127.0.0.1"
enabled = true
audience = "ssh-only"
priority = 0

[[listen]]
kind = "unix"
path = "auto"
enabled = true
audience = "local"
priority = 0

[advertise]
public_host = "prod.example.com"

[auth]
token_file = "auto"
allow_peer_cred = true
"#;

    #[test]
    fn config_parses_all_variants() {
        let cfg: ConfigFile = toml::from_str(FULL_CONFIG).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.runtime_dir, "/var/run/kmux");

        // TLS
        assert_eq!(cfg.tls.cert.as_deref(), Some("/etc/kmuxd/cert.pem"));
        assert_eq!(cfg.tls.key.as_deref(), Some("/etc/kmuxd/key.pem"));
        assert!(!cfg.tls.self_signed);

        // Listeners
        assert_eq!(cfg.listen.len(), 3);
        assert_eq!(cfg.listen[0].kind, ListenKind::Quic);
        assert_eq!(cfg.listen[0].port, 0);
        assert_eq!(cfg.listen[0].audience, Audience::Any);

        assert_eq!(cfg.listen[1].kind, ListenKind::TcpTls);
        assert_eq!(cfg.listen[1].bind, "127.0.0.1");
        assert_eq!(cfg.listen[1].audience, Audience::SshOnly);

        assert_eq!(cfg.listen[2].kind, ListenKind::Unix);
        assert_eq!(cfg.listen[2].audience, Audience::Local);

        // Advertise
        assert_eq!(
            cfg.advertise.public_host.as_deref(),
            Some("prod.example.com")
        );

        // Auth
        assert!(cfg.auth.allow_peer_cred);
    }

    #[test]
    fn defaults_applied_when_fields_absent() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.runtime_dir, "auto");
        assert!(!cfg.tls.self_signed);
        assert_eq!(cfg.listen.len(), 3);
        assert_eq!(cfg.listen[0].kind, ListenKind::Quic);
        assert_eq!(cfg.listen[0].port, 0);
        assert_eq!(cfg.listen[2].audience, Audience::Local); // UDS is local
        assert!(cfg.auth.allow_peer_cred);
    }

    #[test]
    fn audience_ssh_only_parses() {
        let toml = r#"
[[listen]]
kind = "tcp+tls"
audience = "ssh-only"
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(cfg.listen[0].audience, Audience::SshOnly);
    }

    #[test]
    fn self_signed_tls_parses() {
        let toml = r#"
[tls]
self_signed = true
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert!(cfg.tls.self_signed);
    }

    #[test]
    fn server_config_resolve_fails_with_no_enabled_listeners() {
        let mut cfg = ConfigFile::default();
        for l in &mut cfg.listen {
            l.enabled = false;
        }
        assert!(ServerConfig::resolve(cfg).is_err());
    }

    #[test]
    fn server_config_resolve_fails_without_tls_cert() {
        let cfg = ConfigFile {
            tls: TlsConfig {
                cert: None,
                key: None,
                self_signed: false,
            },
            listen: vec![ListenConfig {
                kind: ListenKind::Quic,
                bind: "::".into(),
                port: 0,
                enabled: true,
                path: "auto".into(),
                audience: Audience::Any,
                priority: 0,
            }],
            ..ConfigFile::default()
        };
        assert!(ServerConfig::resolve(cfg).is_err());
    }

    #[test]
    fn server_config_resolve_self_signed_ok() {
        let cfg = ConfigFile {
            tls: TlsConfig {
                self_signed: true,
                ..TlsConfig::default()
            },
            ..ConfigFile::default()
        };
        ServerConfig::resolve(cfg).unwrap();
    }

    #[test]
    fn write_and_read_default_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kmuxd.toml");
        write_default_config(&path).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let cfg: ConfigFile = toml::from_str(&content).unwrap();
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn daemon_idle_shutdown_parses() {
        let toml = r#"
[tls]
self_signed = true

[daemon]
idle_shutdown_secs = 60
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(cfg.daemon.idle_shutdown_secs, 60);
        let resolved = ServerConfig::resolve(cfg).unwrap();
        assert_eq!(resolved.idle_shutdown_secs, 60);
    }

    #[test]
    fn daemon_idle_shutdown_default_is_30() {
        // Only test the ConfigFile default — resolve() requires TLS when QUIC/TCP+TLS are enabled.
        let cfg = ConfigFile::default();
        assert_eq!(cfg.daemon.idle_shutdown_secs, 30);
    }

    #[test]
    fn port_field_rejected_by_toml() {
        // port is #[serde(skip)] so deny_unknown_fields must reject it in TOML.
        let toml = r#"
[[listen]]
kind = "quic"
port = 8443
"#;
        assert!(toml::from_str::<ConfigFile>(toml).is_err());
    }

    #[test]
    fn written_default_config_has_no_port_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kmuxd.toml");
        write_default_config(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("port"),
            "default config must not contain a port field"
        );
    }
}
