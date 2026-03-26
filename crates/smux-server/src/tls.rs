use std::fs;
use std::io::BufReader;
use std::sync::Arc;

use anyhow::{Context, Result};
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// Build a `ServerConfig` from PEM certificate and key files on disk.
pub fn load_tls_config(cert_path: &str, key_path: &str) -> Result<ServerConfig> {
    let cert_file = fs::File::open(cert_path).with_context(|| format!("open cert: {cert_path}"))?;
    let key_file = fs::File::open(key_path).with_context(|| format!("open key: {key_path}"))?;

    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(cert_file))
        .collect::<Result<_, _>>()
        .context("parse certificate PEM")?;

    let key = rustls_pemfile::private_key(&mut BufReader::new(key_file))
        .context("parse private key PEM")?
        .context("no private key found in file")?;

    build_tls_config_from(certs, key)
}

/// Generate an in-memory self-signed certificate and build a `ServerConfig`.
pub fn generate_self_signed() -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>)> {
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let cert = generate_simple_self_signed(subject_alt_names).context("rcgen self-signed")?;

    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    Ok((cert_der, key_der))
}

/// Build a `ServerConfig` from a DER-encoded cert and key pair.
pub fn build_tls_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    build_tls_config_from(vec![cert], key)
}

fn build_tls_config_from(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("build TLS server config")?;
    Ok(config)
}

/// Build a `quinn::ServerConfig` from a rustls `ServerConfig`.
pub fn build_quinn_config(tls_config: ServerConfig) -> Result<quinn::ServerConfig> {
    let server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .context("build QUIC server config from rustls config")?,
    ));
    Ok(server_config)
}
