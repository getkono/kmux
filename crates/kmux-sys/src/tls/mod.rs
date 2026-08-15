//! TLS certificate management: loading from PEM files, self-signed generation,
//! and building a `rustls::ServerConfig`. Shared by QUIC and TCP+TLS transports.
//!
//! Feature-gated on `tls`.

pub mod tofu;
pub mod verifier;

pub use tofu::{Fingerprint, TofuEntry, TofuStore};
pub use verifier::TofuVerifier;

use std::{fs, io::BufReader};

use anyhow::{Context, Result};
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// DER-encoded certificate and private key material.
///
/// Owns all memory — `'static` lifetime throughout.
pub struct CertMaterial {
    pub certs: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Clone for CertMaterial {
    fn clone(&self) -> Self {
        use rustls::pki_types::{PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer};
        let key = match &self.key {
            PrivateKeyDer::Pkcs1(k) => {
                PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(k.secret_pkcs1_der().to_vec()))
            }
            PrivateKeyDer::Sec1(k) => {
                PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(k.secret_sec1_der().to_vec()))
            }
            PrivateKeyDer::Pkcs8(k) => {
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(k.secret_pkcs8_der().to_vec()))
            }
            // Non-exhaustive enum — handle any future variants by serializing raw DER bytes.
            key => PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.secret_der().to_vec())),
        };
        Self {
            certs: self.certs.clone(),
            key,
        }
    }
}

impl CertMaterial {
    /// Load certificate and private key from PEM files on disk.
    pub fn from_files(cert_path: &str, key_path: &str) -> Result<Self> {
        let cert_file =
            fs::File::open(cert_path).with_context(|| format!("open cert: {cert_path}"))?;
        let key_file = fs::File::open(key_path).with_context(|| format!("open key: {key_path}"))?;

        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut BufReader::new(cert_file))
                .collect::<Result<_, _>>()
                .context("parse certificate PEM")?;

        let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
            .context("parse private key PEM")?
            .context("no private key found in file")?;

        Ok(Self { certs, key })
    }

    /// Generate an in-memory self-signed certificate valid for `localhost` and `127.0.0.1`.
    pub fn self_signed() -> Result<Self> {
        let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let cert =
            generate_simple_self_signed(subject_alt_names).context("rcgen self-signed cert")?;

        let certs = vec![CertificateDer::from(cert.cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));

        Ok(Self { certs, key })
    }
}

/// Build a `rustls::ServerConfig` from `CertMaterial`.
///
/// No client authentication is required.
pub fn build_server_config(material: CertMaterial) -> Result<ServerConfig> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(material.certs, material.key)
        .context("build rustls ServerConfig")?;
    Ok(config)
}
