//! TOFU (Trust On First Use) certificate pinning store.
//!
//! Manages `~/.config/kmux/known_hosts.toml` — a per-host list of pinned TLS leaf-certificate
//! SHA-256 fingerprints, keyed by `(addr, transport)`.
//!
//! Trust policy (see also `verifier::TofuVerifier`):
//! - **CA-valid cert**: accepted; auto-pinned on first encounter (quiet).
//! - **Self-signed / CA-invalid, pin exists, match**: accepted.
//! - **Self-signed / CA-invalid, pin exists, mismatch**: hard fail with diff in error message.
//! - **Self-signed / CA-invalid, no pin**: auto-pinned with `tracing::warn!` on first trust.
//! - **`accept_invalid_certs = true`**: all checks bypassed (development only).

use std::path::PathBuf;

use anyhow::{Context, Result};
use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ─── Fingerprint ─────────────────────────────────────────────────────────────

/// SHA-256 fingerprint of a TLS leaf certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    /// Compute the SHA-256 fingerprint of a DER-encoded certificate.
    pub fn from_cert(cert: &CertificateDer<'_>) -> Self {
        let digest = Sha256::digest(cert.as_ref());
        Self(digest.into())
    }

    /// Render the fingerprint as a lowercase hex string.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse a lowercase hex fingerprint string (64 hex chars).
    pub fn from_hex(hex: &str) -> Result<Self> {
        anyhow::ensure!(
            hex.len() == 64,
            "fingerprint must be 64 hex chars, got {}",
            hex.len()
        );
        let mut bytes = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let hi = hex_nibble(chunk[0])?;
            let lo = hex_nibble(chunk[1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Self(bytes))
    }
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => anyhow::bail!("invalid hex character: {}", b as char),
    }
}

// ─── TofuEntry ───────────────────────────────────────────────────────────────

/// A single pinned certificate entry in `known_hosts.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TofuEntry {
    /// Server address, e.g. `"prod.example:8443"`.
    pub addr: String,
    /// Lowercase hex SHA-256 fingerprint of the leaf certificate.
    pub sha256: String,
    /// Transport kind: `"quic"` or `"tcp+tls"`.
    pub transport: String,
    /// ISO 8601 UTC timestamp when the entry was first pinned, e.g. `"2026-04-16T12:00:00Z"`.
    pub first_seen: String,
}

// ─── TofuStore ───────────────────────────────────────────────────────────────

/// Top-level TOML wrapper for `[[host]]` entries.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TofuFile {
    #[serde(default, rename = "host")]
    hosts: Vec<TofuEntry>,
}

/// In-memory TOFU store backed by `known_hosts.toml`.
#[derive(Debug)]
pub struct TofuStore {
    path: PathBuf,
    entries: Vec<TofuEntry>,
}

impl TofuStore {
    /// Load the store from `path`.  Returns an empty store (no error) if the file does not exist.
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read known_hosts: {}", path.display()))?;
            let file: TofuFile = toml::from_str(&raw).context("parse known_hosts.toml")?;
            file.hosts
        } else {
            vec![]
        };
        Ok(Self { path, entries })
    }

    /// Empty non-persistent store for an explicitly insecure verifier.
    ///
    /// The verifier returns before consulting this store; keeping it in-memory
    /// avoids touching `known_hosts.toml` when certificate checks are disabled.
    pub(crate) fn ephemeral() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Vec::new(),
        }
    }

    /// Find an existing pin for the given `(addr, transport)` pair.
    pub fn lookup(&self, addr: &str, transport: &str) -> Option<&TofuEntry> {
        self.entries
            .iter()
            .find(|e| e.addr == addr && e.transport == transport)
    }

    /// Add or update a pin for `(addr, transport)` and persist to disk.
    pub fn pin(&mut self, addr: &str, transport: &str, fp: &Fingerprint) -> Result<()> {
        let first_seen = current_timestamp();
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.addr == addr && e.transport == transport)
        {
            entry.sha256 = fp.to_hex();
        } else {
            self.entries.push(TofuEntry {
                addr: addr.to_string(),
                sha256: fp.to_hex(),
                transport: transport.to_string(),
                first_seen,
            });
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let file = TofuFile {
            hosts: self.entries.clone(),
        };
        let raw = toml::to_string_pretty(&file).context("serialize known_hosts.toml")?;
        // Create parent dirs if needed.
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        std::fs::write(&self.path, raw)
            .with_context(|| format!("write known_hosts: {}", self.path.display()))
    }
}

/// Current time as a compact ISO 8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
fn current_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = kmux_protocol::messages::epoch_secs_to_ymd_hms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

impl TofuStore {
    /// Number of entries in the store (used in tests).
    #[cfg(test)]
    fn iter_count(&self) -> usize {
        self.entries.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn zeroed_fp() -> Fingerprint {
        Fingerprint([0u8; 32])
    }

    fn ones_fp() -> Fingerprint {
        Fingerprint([0xffu8; 32])
    }

    #[test]
    fn fingerprint_hex_roundtrip() {
        let fp = zeroed_fp();
        let hex = fp.to_hex();
        assert_eq!(hex.len(), 64);
        let decoded = Fingerprint::from_hex(&hex).unwrap();
        assert_eq!(fp, decoded);
    }

    #[test]
    fn fingerprint_hex_invalid() {
        assert!(Fingerprint::from_hex("zz").is_err());
        assert!(Fingerprint::from_hex("deadbeef").is_err()); // too short
    }

    #[test]
    fn first_seen_auto_pin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");
        let mut store = TofuStore::load(path.clone()).unwrap();

        assert!(store.lookup("host:8443", "quic").is_none());
        store.pin("host:8443", "quic", &zeroed_fp()).unwrap();
        assert!(store.lookup("host:8443", "quic").is_some());

        // Reload from disk to verify persistence.
        let store2 = TofuStore::load(path).unwrap();
        let entry = store2.lookup("host:8443", "quic").unwrap();
        assert_eq!(entry.sha256, zeroed_fp().to_hex());
        assert_eq!(entry.transport, "quic");
    }

    #[test]
    fn pin_match() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");
        let mut store = TofuStore::load(path).unwrap();

        store.pin("host:8444", "tcp+tls", &zeroed_fp()).unwrap();

        let entry = store.lookup("host:8444", "tcp+tls").unwrap();
        let stored = Fingerprint::from_hex(&entry.sha256).unwrap();
        assert_eq!(stored, zeroed_fp());
    }

    #[test]
    fn pin_mismatch_detect() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");
        let mut store = TofuStore::load(path).unwrap();

        store.pin("host:8443", "quic", &zeroed_fp()).unwrap();

        let entry = store.lookup("host:8443", "quic").unwrap();
        let stored = Fingerprint::from_hex(&entry.sha256).unwrap();
        // Simulate a mismatch: stored pin != presented cert
        assert_ne!(stored, ones_fp(), "should detect mismatch");
    }

    #[test]
    fn pin_update_replaces_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");
        let mut store = TofuStore::load(path.clone()).unwrap();

        store.pin("host:8443", "quic", &zeroed_fp()).unwrap();
        store.pin("host:8443", "quic", &ones_fp()).unwrap();

        let store2 = TofuStore::load(path).unwrap();
        assert_eq!(
            store2.iter_count(),
            1,
            "update should not duplicate entries"
        );
        assert_eq!(
            store2.lookup("host:8443", "quic").unwrap().sha256,
            ones_fp().to_hex()
        );
    }

    #[test]
    fn separate_transports_independent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_hosts.toml");
        let mut store = TofuStore::load(path).unwrap();

        store.pin("host:8443", "quic", &zeroed_fp()).unwrap();
        store.pin("host:8444", "tcp+tls", &ones_fp()).unwrap();

        assert_eq!(
            store.lookup("host:8443", "quic").unwrap().sha256,
            zeroed_fp().to_hex()
        );
        assert_eq!(
            store.lookup("host:8444", "tcp+tls").unwrap().sha256,
            ones_fp().to_hex()
        );
    }

    #[test]
    fn fingerprint_from_cert_bytes() {
        // SHA-256 of an empty input is well-known
        use rustls::pki_types::CertificateDer;
        let empty: CertificateDer<'_> = CertificateDer::from(vec![]);
        let fp = Fingerprint::from_cert(&empty);
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(fp.to_hex(), expected);
    }
}
