use std::sync::Arc;

use anyhow::{Context, Result};

// Re-export CertMaterial and server-config builder from the shared protocol crate.
pub use kmux_sys::tls::{CertMaterial, build_server_config};

/// Build a `quinn::ServerConfig` from a `rustls::ServerConfig`.
///
/// Applies the shared QUIC idle-timeout and keep-alive intervals from
/// `kmux_protocol::timing`.
pub fn build_quinn_config(tls_config: rustls::ServerConfig) -> Result<quinn::ServerConfig> {
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls_config)
            .context("build QUIC server config from rustls config")?,
    ));

    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(kmux_protocol::timing::QUIC_IDLE_TIMEOUT).unwrap(),
    ));
    transport.keep_alive_interval(Some(kmux_protocol::timing::QUIC_KEEP_ALIVE));
    server_config.transport_config(Arc::new(transport));

    Ok(server_config)
}
