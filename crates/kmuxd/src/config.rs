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

use kmux_protocol::TransportKind;
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

    /// Wire compression settings.
    #[serde(default)]
    pub compression: CompressionConfig,

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
            compression: CompressionConfig::default(),
            daemon: DaemonConfig::default(),
        }
    }
}

/// Pane VT-pipeline isolation mode (`[daemon] session_isolation` in `kmuxd.toml`,
/// overridable with `kmuxd --session-isolation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum SessionIsolationMode {
    /// The emulator (`TermState`) and PTY writer live in the daemon. The default
    /// and historical behavior.
    #[default]
    InProcess,
    /// Each pane's VT pipeline runs in an isolated `kmux-vt-worker` subprocess so
    /// a libghostty-vt crash cannot take down the daemon (issue #126).
    Process,
}

impl SessionIsolationMode {
    /// Whether per-pane worker subprocess isolation is requested.
    pub fn is_process(self) -> bool {
        matches!(self, SessionIsolationMode::Process)
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
    /// Pane isolation mode (issue #126). `in-process` (default) keeps the
    /// emulator in the daemon; `process` runs each pane's VT pipeline in an
    /// isolated `kmux-vt-worker` subprocess.
    #[serde(default)]
    pub session_isolation: SessionIsolationMode,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_shutdown_secs: default_idle_shutdown_secs(),
            session_isolation: SessionIsolationMode::default(),
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
    /// Path to the PEM certificate file. When no `cert`/`key` pair is set the
    /// daemon generates an in-memory self-signed certificate (the default).
    pub cert: Option<String>,
    /// Path to the PEM private key file. When no `cert`/`key` pair is set the
    /// daemon generates an in-memory self-signed certificate (the default).
    pub key: Option<String>,
    /// Deprecated and ignored. Self-signed certificates are now the default
    /// whenever no `cert`/`key` pair is configured (issue #100), so this knob
    /// no longer changes any behaviour. It is still accepted — and skipped on
    /// serialize — purely so config files that predate the change continue to
    /// parse under `deny_unknown_fields`.
    #[serde(default, skip_serializing)]
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
    // Use `default_bind()` (0.0.0.0) rather than a hardcoded "::": the latter
    // formats to the unparseable `:::0` for the kernel-assigned (port 0)
    // listeners and, per `default_bind`'s rationale, silently drops IPv4 on
    // `bindv6only` hosts. Operators who want IPv6 can still set `bind = "::"`.
    vec![
        ListenConfig {
            kind: ListenKind::Quic,
            bind: default_bind(),
            port: 0,
            enabled: true,
            path: "auto".to_string(),
            audience: Audience::Any,
            priority: 0,
        },
        ListenConfig {
            kind: ListenKind::TcpTls,
            bind: default_bind(),
            port: 0,
            enabled: true,
            path: "auto".to_string(),
            audience: Audience::Any,
            priority: 0,
        },
        ListenConfig {
            kind: ListenKind::Unix,
            bind: default_bind(),
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

/// Wire compression configuration (`[compression]` in `kmuxd.toml`).
///
/// The daemon decides per connection whether to compress server→client traffic;
/// see `docs/compression.md`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionConfig {
    /// When to compress. See [`CompressionMode`].
    #[serde(default)]
    pub mode: CompressionMode,
    /// zstd compression level applied by the sender. The decoder reconstructs
    /// it from the zstd frame, so it never appears on the wire.
    #[serde(default = "default_compression_level")]
    pub level: i32,
    /// Frames smaller than this many bytes are never compressed: the zstd
    /// overhead would dominate and tiny frames rarely shrink.
    #[serde(default = "default_compression_min_size")]
    pub min_size: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            mode: CompressionMode::default(),
            level: default_compression_level(),
            min_size: default_compression_min_size(),
        }
    }
}

fn default_compression_level() -> i32 {
    3
}
fn default_compression_min_size() -> usize {
    256
}

/// When the daemon compresses server→client traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionMode {
    /// Compress non-local clients; leave local (UDS) clients uncompressed.
    /// This is the issue #59 default ("on if the client is not local").
    #[default]
    Auto,
    /// Always compress, regardless of locality.
    Always,
    /// Never compress.
    Never,
}

impl CompressionConfig {
    /// Whether compression should be enabled for a connection accepted on
    /// `transport`, per the configured [`CompressionMode`].
    pub fn enabled_for(&self, transport: TransportKind) -> bool {
        match self.mode {
            CompressionMode::Always => true,
            CompressionMode::Never => false,
            // A Unix-domain socket is same-host: bandwidth is free there, so
            // spending CPU to compress is pure waste. Every other transport may
            // cross a network and benefits from compression.
            CompressionMode::Auto => transport != TransportKind::Uds,
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
    pub compression: CompressionConfig,
    pub runtime_dir: String,
    /// Seconds of inactivity (zero clients) before the daemon exits. `0` = disabled.
    pub idle_shutdown_secs: u64,
    /// Pane VT-pipeline isolation mode (issue #126).
    pub session_isolation: SessionIsolationMode,
}

impl ServerConfig {
    /// Validate the config and return a resolved `ServerConfig`.
    pub fn resolve(file: ConfigFile) -> anyhow::Result<Self> {
        // At least one listener must be enabled.
        if !file.listen.iter().any(|l| l.enabled) {
            anyhow::bail!("no enabled listeners in configuration");
        }

        // A custom TLS certificate needs both halves. When neither is set the
        // daemon falls back to an in-memory self-signed certificate — the
        // default for this kind of software, so no flag or config knob is
        // required (issue #100). Only a half-configured pair is an error.
        match (file.tls.cert.is_some(), file.tls.key.is_some()) {
            (true, false) => anyhow::bail!("[tls] cert is set but [tls] key is missing"),
            (false, true) => anyhow::bail!("[tls] key is set but [tls] cert is missing"),
            _ => {}
        }

        // The `self_signed` knob is gone (issue #100); a value lingering in an
        // older config file is accepted but does nothing. Nudge the operator to
        // drop it instead of silently ignoring it.
        if file.tls.self_signed {
            tracing::warn!(
                "[tls] self_signed is deprecated and ignored: self-signed certificates are now \
                 the default whenever no cert/key pair is configured; remove it from kmuxd.toml"
            );
        }

        Ok(Self {
            tls: file.tls,
            listeners: file.listen,
            advertise: file.advertise,
            auth: file.auth,
            compression: file.compression,
            runtime_dir: file.runtime_dir,
            idle_shutdown_secs: file.daemon.idle_shutdown_secs,
            session_isolation: file.daemon.session_isolation,
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
    fn server_config_resolve_ok_without_tls_cert() {
        // No cert/key configured → self-signed is the default, so resolve must
        // succeed even with QUIC/TCP+TLS listeners enabled (issue #100).
        let cfg = ConfigFile {
            tls: TlsConfig::default(),
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
        ServerConfig::resolve(cfg).unwrap();
    }

    #[test]
    fn server_config_resolve_fails_with_half_set_cert_pair() {
        // A cert without its key (or vice versa) is a misconfiguration.
        let cert_only = ConfigFile {
            tls: TlsConfig {
                cert: Some("/etc/kmuxd/cert.pem".into()),
                key: None,
                ..TlsConfig::default()
            },
            ..ConfigFile::default()
        };
        assert!(ServerConfig::resolve(cert_only).is_err());

        let key_only = ConfigFile {
            tls: TlsConfig {
                cert: None,
                key: Some("/etc/kmuxd/key.pem".into()),
                ..TlsConfig::default()
            },
            ..ConfigFile::default()
        };
        assert!(ServerConfig::resolve(key_only).is_err());
    }

    #[test]
    fn server_config_resolve_accepts_deprecated_self_signed() {
        // The deprecated `self_signed` knob is ignored but must still resolve
        // cleanly so legacy config files keep working (issue #100).
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
    fn daemon_session_isolation_parses() {
        let toml = r#"
[tls]
self_signed = true

[daemon]
session_isolation = "process"
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(cfg.daemon.session_isolation, SessionIsolationMode::Process);
        let resolved = ServerConfig::resolve(cfg).unwrap();
        assert!(resolved.session_isolation.is_process());
    }

    #[test]
    fn daemon_session_isolation_defaults_to_in_process() {
        let cfg = ConfigFile::default();
        assert_eq!(
            cfg.daemon.session_isolation,
            SessionIsolationMode::InProcess
        );
        assert!(!cfg.daemon.session_isolation.is_process());
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

    #[test]
    fn compression_defaults_are_auto_level3() {
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.mode, CompressionMode::Auto);
        assert_eq!(cfg.level, 3);
        assert_eq!(cfg.min_size, 256);
    }

    #[test]
    fn compression_section_parses() {
        let toml = r#"
[compression]
mode = "always"
level = 9
min_size = 128
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(cfg.compression.mode, CompressionMode::Always);
        assert_eq!(cfg.compression.level, 9);
        assert_eq!(cfg.compression.min_size, 128);
    }

    #[test]
    fn compression_enabled_for_transport() {
        let auto = CompressionConfig::default();
        // Auto: on for every networked transport, off for the local UDS path.
        assert!(auto.enabled_for(TransportKind::Quic));
        assert!(auto.enabled_for(TransportKind::Tcp));
        assert!(auto.enabled_for(TransportKind::TcpTls));
        assert!(!auto.enabled_for(TransportKind::Uds));

        let always = CompressionConfig {
            mode: CompressionMode::Always,
            ..CompressionConfig::default()
        };
        assert!(always.enabled_for(TransportKind::Uds));

        let never = CompressionConfig {
            mode: CompressionMode::Never,
            ..CompressionConfig::default()
        };
        assert!(!never.enabled_for(TransportKind::Quic));
    }

    #[test]
    fn written_default_config_has_no_self_signed_field() {
        // `self_signed` is deprecated and `skip_serializing`, so freshly written
        // configs must not mention it (issue #100).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kmuxd.toml");
        write_default_config(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("self_signed"),
            "default config must not contain a self_signed field"
        );
    }
}
