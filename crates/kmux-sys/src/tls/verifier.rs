//! `TofuVerifier` — a `rustls::client::danger::ServerCertVerifier` implementation that
//! applies Trust On First Use (TOFU) certificate pinning with system-CA fallback.
//!
//! Verification policy (all in a single `verify_server_cert` call):
//! 1. `accept_invalid_certs = true` → always accept (development escape hatch).
//! 2. Try system/native root CA chain.  If CA-valid → auto-pin (quiet) + accept.
//! 3. CA-invalid + existing pin → compare fingerprints; mismatch = hard fail.
//! 4. CA-invalid + no pin → auto-pin with `tracing::warn!` + accept.
//!
//! Pin-store lock or persistence failures are hard failures unless certificate
//! validation was explicitly disabled before the store was loaded.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use tracing::warn;

use super::tofu::{Fingerprint, TofuStore};

// ─── TofuVerifier ────────────────────────────────────────────────────────────

/// A `ServerCertVerifier` that applies TOFU certificate pinning with system-CA fallback.
#[derive(Debug)]
pub struct TofuVerifier {
    /// Server address used as the TOFU store key, e.g. `"prod.example:8443"`.
    addr: String,
    /// Transport kind used as the TOFU store key: `"quic"` or `"tcp+tls"`.
    transport: &'static str,
    /// Shared TOFU store (pinned fingerprints persisted in `known_hosts.toml`).
    store: Arc<Mutex<TofuStore>>,
    /// Skip all certificate checks; use only in development with self-signed certs.
    accept_invalid_certs: bool,
    /// Inner verifier backed by system/native root CAs.  `None` if no roots were loaded.
    inner: Option<Arc<dyn ServerCertVerifier>>,
}

impl TofuVerifier {
    /// Build a strict verifier that loads native root CAs automatically.
    pub fn new(addr: String, transport: &'static str, store: Arc<Mutex<TofuStore>>) -> Self {
        let inner = build_native_ca_verifier();
        Self {
            addr,
            transport,
            store,
            accept_invalid_certs: false,
            inner,
        }
    }

    /// Build an explicitly insecure verifier without loading or persisting pins.
    pub fn accept_invalid(addr: String, transport: &'static str) -> Self {
        Self {
            addr,
            transport,
            store: Arc::new(Mutex::new(TofuStore::ephemeral())),
            accept_invalid_certs: true,
            inner: None,
        }
    }

    /// Build a TOFU-only verifier with no CA validation (useful in tests).
    #[cfg(test)]
    pub(crate) fn tofu_only(
        addr: String,
        transport: &'static str,
        store: Arc<Mutex<TofuStore>>,
    ) -> Self {
        Self {
            addr,
            transport,
            store,
            accept_invalid_certs: false,
            inner: None,
        }
    }
}

impl ServerCertVerifier for TofuVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if self.accept_invalid_certs {
            return Ok(ServerCertVerified::assertion());
        }

        let fp = Fingerprint::from_cert(end_entity);
        let mut store = self
            .store
            .lock()
            .map_err(|_| Error::General("TOFU store lock poisoned".to_string()))?;

        // Step 1 — Try system CA validation.
        let ca_valid = self.inner.as_ref().is_some_and(|inner| {
            inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .is_ok()
        });

        if ca_valid {
            // Auto-pin CA-valid certs silently on first encounter.
            if store.lookup(&self.addr, self.transport).is_none() {
                store.pin(&self.addr, self.transport, &fp).map_err(|e| {
                    Error::General(format!("failed to persist certificate pin: {e}"))
                })?;
            }
            return Ok(ServerCertVerified::assertion());
        }

        // Step 2 — CA validation failed; consult TOFU store.
        let pinned_hex = store
            .lookup(&self.addr, self.transport)
            .map(|entry| entry.sha256.clone());

        match pinned_hex {
            None => {
                // No existing pin — auto-pin on first trust with a visible warning.
                warn!(
                    addr = %self.addr,
                    transport = %self.transport,
                    fingerprint = %fp.to_hex(),
                    "TOFU: pinning certificate on first trust; verify fingerprint out-of-band"
                );
                store.pin(&self.addr, self.transport, &fp).map_err(|e| {
                    Error::General(format!("failed to persist certificate pin: {e}"))
                })?;
                Ok(ServerCertVerified::assertion())
            }
            Some(hex) => {
                let pinned_fp = Fingerprint::from_hex(&hex)
                    .map_err(|e| Error::General(format!("invalid pinned fingerprint: {e}")))?;
                if pinned_fp == fp {
                    Ok(ServerCertVerified::assertion())
                } else {
                    Err(Error::General(format!(
                        "TLS fingerprint mismatch for {}. \
                         Pinned: {}  Presented: {}  \
                         If the server certificate was legitimately rotated, \
                         update the entry in ~/.config/kmux/known_hosts.toml.",
                        self.addr,
                        pinned_fp.to_hex(),
                        fp.to_hex(),
                    )))
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dh_params: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if self.accept_invalid_certs {
            return Ok(HandshakeSignatureValid::assertion());
        }
        let provider = CryptoProvider::get_default()
            .ok_or_else(|| Error::General("no default CryptoProvider installed".to_string()))?;
        verify_tls12_signature(
            message,
            cert,
            dh_params,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dh_params: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        if self.accept_invalid_certs {
            return Ok(HandshakeSignatureValid::assertion());
        }
        let provider = CryptoProvider::get_default()
            .ok_or_else(|| Error::General("no default CryptoProvider installed".to_string()))?;
        verify_tls13_signature(
            message,
            cert,
            dh_params,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a `ServerCertVerifier` backed by the system's native root CAs.
/// Returns `None` if no roots were loaded or the verifier could not be built.
fn build_native_ca_verifier() -> Option<Arc<dyn ServerCertVerifier>> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        if let Err(e) = roots.add(cert) {
            warn!("skipping native root cert: {e}");
        }
    }
    if roots.is_empty() {
        warn!("no native root CA certificates loaded; CA validation disabled");
        return None;
    }
    rustls::client::WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .ok()
        .map(|v| -> Arc<dyn ServerCertVerifier> { v })
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::tofu::TofuStore;
    use rcgen::generate_simple_self_signed;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tempfile::tempdir;

    fn self_signed_cert() -> CertificateDer<'static> {
        let cert = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        CertificateDer::from(cert.cert.der().to_vec())
    }

    fn dummy_server_name() -> ServerName<'static> {
        "localhost".try_into().unwrap()
    }

    fn now() -> UnixTime {
        UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        ))
    }

    fn make_verifier(dir: &tempfile::TempDir) -> TofuVerifier {
        let path = dir.path().join("known_hosts.toml");
        let store = Arc::new(Mutex::new(TofuStore::load(path).unwrap()));
        TofuVerifier::tofu_only("localhost:8443".to_string(), "quic", store)
    }

    #[test]
    fn accept_invalid_certs_bypass() {
        let v = TofuVerifier::accept_invalid("localhost:8443".to_string(), "quic");
        let cert = self_signed_cert();
        let result = v.verify_server_cert(&cert, &[], &dummy_server_name(), &[], now());
        assert!(
            result.is_ok(),
            "accept_invalid_certs should bypass all checks"
        );
    }

    #[test]
    fn first_trust_rejects_when_pin_cannot_be_persisted() {
        let dir = tempdir().unwrap();
        let blocked_parent = dir.path().join("not-a-directory");
        std::fs::write(&blocked_parent, b"block create_dir_all").unwrap();
        let store = Arc::new(Mutex::new(
            TofuStore::load(blocked_parent.join("known_hosts.toml")).unwrap(),
        ));
        let verifier = TofuVerifier::tofu_only("localhost:8443".into(), "quic", store);

        let result =
            verifier.verify_server_cert(&self_signed_cert(), &[], &dummy_server_name(), &[], now());
        let error = result.expect_err("an unpersisted first pin must be rejected");
        assert!(
            error
                .to_string()
                .contains("failed to persist certificate pin")
        );
    }

    #[test]
    fn invalid_without_pin_auto_pins() {
        let dir = tempdir().unwrap();
        let v = make_verifier(&dir);
        let cert = self_signed_cert();

        // First call: no pin → auto-pin + accept
        let result = v.verify_server_cert(&cert, &[], &dummy_server_name(), &[], now());
        assert!(
            result.is_ok(),
            "first-trust should be accepted and auto-pinned"
        );

        let store = v.store.lock().unwrap();
        assert!(
            store.lookup("localhost:8443", "quic").is_some(),
            "cert should be pinned after first-trust"
        );
    }

    #[test]
    fn pin_match_accepted() {
        let dir = tempdir().unwrap();
        let v = make_verifier(&dir);
        let cert = self_signed_cert();

        // Auto-pin on first call
        v.verify_server_cert(&cert, &[], &dummy_server_name(), &[], now())
            .unwrap();

        // Second call with same cert: pin match → accepted
        let result = v.verify_server_cert(&cert, &[], &dummy_server_name(), &[], now());
        assert!(result.is_ok(), "pin match should be accepted");
    }

    #[test]
    fn pin_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let v = make_verifier(&dir);
        let cert1 = self_signed_cert();
        let cert2 = self_signed_cert(); // different key pair → different fingerprint

        // Pin cert1
        v.verify_server_cert(&cert1, &[], &dummy_server_name(), &[], now())
            .unwrap();

        // Present cert2 — fingerprints differ → hard fail
        let result = v.verify_server_cert(&cert2, &[], &dummy_server_name(), &[], now());
        assert!(result.is_err(), "fingerprint mismatch must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("fingerprint mismatch"),
            "error should mention mismatch: {err}"
        );
    }
}
