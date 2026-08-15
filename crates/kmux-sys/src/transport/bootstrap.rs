//! Bootstrap abstractions: `Bootstrap` trait, `SessionContext`, `EndpointAdvert`.
//!
//! Phase 5: defines the two-phase connection model.
//!
//! Phase A (bootstrap): obtain an authenticated `SessionContext` via any reachable path.
//! Phase B (data plane): `TransportSupervisor` uses `server_endpoints` from the context
//!                       to select and hot-swap the best long-lived transport.
//!
//! The `Bootstrap` trait is object-safe so strategies can be stored as
//! `Vec<Box<dyn Bootstrap>>` and run concurrently via `FuturesUnordered`.

use std::future::Future;
use std::pin::Pin;

use thiserror::Error;
use tokio::sync::mpsc;

use kmux_protocol::messages::{ClientMessage, ConnectionId, ServerMessage, TransportKind};

// ─── EndpointAdvert ──────────────────────────────────────────────────────────

/// A transport endpoint advertised by the server after authentication.
///
/// Populated from `AuthResult` (or a future `StatusResponse`). The client
/// uses this list to open and rank data-plane transports in Phase B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAdvert {
    /// Transport kind for this endpoint.
    pub kind: TransportKind,
    /// Connection address: `"host:port"` for QUIC/TLS-TCP, absolute path for UDS.
    pub address: String,
}

// ─── SessionContext ──────────────────────────────────────────────────────────

/// All information produced by a successful bootstrap.
///
/// Handed to `TransportSupervisor` (Phase 6) which uses `server_endpoints`
/// to open the best data-plane transport. The `send` channel is the already-
/// authenticated initial channel (often the bootstrap channel itself, reused
/// as the data plane until something better arrives).
#[derive(Debug)]
pub struct SessionContext {
    /// Auth token echoed from the server (may be refreshed on reconnect).
    pub token: String,
    /// Connection identity assigned by the server; persist across transport swaps.
    pub connection_id: ConnectionId,
    /// Endpoints the server advertises for data-plane use.
    pub server_endpoints: Vec<EndpointAdvert>,
    /// Which transport was used during bootstrap (accounting only).
    pub bootstrap_transport: TransportKind,
    /// Sender for the already-connected, already-authenticated initial channel.
    pub send: mpsc::UnboundedSender<ClientMessage>,
}

// ─── BootstrapError ──────────────────────────────────────────────────────────

/// Errors from a bootstrap strategy.
#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("authentication failed: {reason}")]
    AuthFailed { reason: String },

    #[error("protocol version mismatch: client={client}, server={server}")]
    VersionMismatch { client: u32, server: u32 },

    #[error("kmuxd is not installed on the remote host")]
    RemoteNotInstalled,

    #[error("this strategy is not applicable for the given target")]
    NotAvailable,

    #[error("all bootstrap strategies failed: {0:?}")]
    AllFailed(Vec<String>),
}

// ─── Bootstrap trait ─────────────────────────────────────────────────────────

/// A single bootstrap strategy.
///
/// Implementations are object-safe: `try_bootstrap` returns a boxed future so
/// strategies can be stored as `Vec<Box<dyn Bootstrap>>` and polled concurrently
/// via `futures::stream::FuturesUnordered`.
///
/// Each strategy is responsible for:
/// 1. Establishing a transport connection.
/// 2. Sending `ClientMessage::Auth` and reading `ServerMessage::AuthResult`.
/// 3. Extracting `connection_id` from a successful `AuthResult`.
/// 4. Forwarding the `AuthResult` message via `server_tx` so `SessionManager`
///    sees it and sets up `client_id`, `server_version`, etc.
/// 5. Spawning reader/writer tasks for ongoing message delivery.
/// 6. Returning a `SessionContext` with the extracted context.
pub trait Bootstrap: Send + Sync + 'static {
    /// Short human-readable name used in log messages and error reports.
    fn name(&self) -> &'static str;

    /// Attempt bootstrap; forward all server messages to `server_tx`.
    ///
    /// Returns `Err(BootstrapError::NotAvailable)` when this strategy is
    /// inapplicable (e.g. `UdsLocalBootstrap` when no daemon is running).
    fn try_bootstrap(
        &self,
        server_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Pin<Box<dyn Future<Output = Result<SessionContext, BootstrapError>> + Send + '_>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::TransportKind;

    #[test]
    fn endpoint_advert_equality() {
        let a = EndpointAdvert {
            kind: TransportKind::Quic,
            address: "host:8443".to_string(),
        };
        let b = EndpointAdvert {
            kind: TransportKind::Quic,
            address: "host:8443".to_string(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn bootstrap_error_display() {
        let e = BootstrapError::VersionMismatch {
            client: 13,
            server: 14,
        };
        assert!(e.to_string().contains("13"));
        assert!(e.to_string().contains("14"));
    }

    #[test]
    fn connection_failed_display() {
        let e = BootstrapError::ConnectionFailed("refused".to_string());
        assert!(e.to_string().contains("refused"));
    }
}
