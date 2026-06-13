//! Toolkit-neutral snapshot of the live connection's technical details, shown
//! by the GUI **connection inspector** (the sibling of the metrics inspector,
//! issue #60).
//!
//! Built in one place from the
//! [`SessionManager`](kmux_client::session_manager::SessionManager) plus
//! [`AppCore`] state so every frontend renders the same field set: `kmux-gtk`
//! reads this struct directly, `kmux-swift` maps it across the `kmux-ffi`
//! boundary. See `docs/connection.md`.

use kmux_protocol::messages::PROTOCOL_VERSION;

use super::AppCore;

/// Recent round-trip-time summary for the active transport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RttInfo {
    /// Exponentially-weighted moving average (ms), `None` before any sample.
    pub ewma_ms: Option<f64>,
    /// Mean of the recent rolling window (ms).
    pub recent_avg_ms: f64,
    /// Max of the recent rolling window (ms).
    pub recent_max_ms: f64,
    /// Total Ping/Pong samples observed on this transport.
    pub samples: u64,
}

/// Byte/message traffic totals for one `(transport, endpoint)` bucket.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransportTraffic {
    /// Human label, e.g. `"QUIC 1.2.3.4:8443"`.
    pub label: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
}

/// A point-in-time snapshot of the connection / session / handshake details
/// rendered by the connection inspector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectionInfo {
    /// User-facing server label (`localhost` or `user@host`).
    pub server: String,
    /// Whether this is the local UDS daemon (no TLS, no network endpoint).
    pub is_local: bool,
    /// Resolved data-plane endpoint `host:port` (empty before first connect).
    pub endpoint: String,
    /// High-level connection-state badge (`CONNECTED · QUIC`, `RECONNECTING #2`…).
    pub state: String,
    /// Whether traffic is currently flowing.
    pub connected: bool,
    /// Active transport channel label (`QUIC`/`TCP+TLS`/`UDS`/`TCP`).
    pub transport: String,
    /// Server-assigned connection identity (persists across transport swaps).
    pub connection_id: Option<u64>,
    /// Server-assigned client identity (per attached client).
    pub client_id: Option<u64>,
    /// Daemon binary version reported at auth.
    pub server_version: Option<String>,
    /// Wire-protocol version this client speaks.
    pub protocol_version: u32,
    /// True when the client accepts invalid TLS certs (dev / self-signed).
    pub accept_invalid_certs: bool,
    /// Latency summary for the active transport.
    pub rtt: Option<RttInfo>,
    /// Per-transport traffic totals observed this connection.
    pub transports: Vec<TransportTraffic>,
}

impl AppCore {
    /// Gather the live connection's technical details for the connection
    /// inspector. Cheap (a few field reads + a small map); safe to call each
    /// frame the inspector is open.
    pub fn connection_info(&self) -> ConnectionInfo {
        let mgr = &self.mgr;
        let rtt = mgr.active_rtt().map(|s| RttInfo {
            ewma_ms: s.ewma_ms,
            recent_avg_ms: s.recent_avg_ms,
            recent_max_ms: s.recent_max_ms,
            samples: s.sample_count as u64,
        });
        let transports = mgr
            .metrics
            .network
            .snapshot_by_transport()
            .into_iter()
            .map(|(key, c)| TransportTraffic {
                label: format!("{} {}", key.kind, key.address),
                bytes_in: c.bytes_in,
                bytes_out: c.bytes_out,
                msgs_in: c.msgs_in,
                msgs_out: c.msgs_out,
            })
            .collect();
        ConnectionInfo {
            server: if self.is_local {
                "localhost".to_string()
            } else {
                self.server_display.clone()
            },
            is_local: self.is_local,
            endpoint: mgr.host_port_display(),
            state: mgr.connection_state().badge_label(),
            connected: mgr.is_connected(),
            transport: mgr.current_transport().to_string(),
            connection_id: mgr.connection_id.map(|c| c.0),
            client_id: mgr.client_id().map(|c| c.0),
            server_version: mgr.server_version.clone(),
            protocol_version: PROTOCOL_VERSION,
            accept_invalid_certs: mgr.accept_invalid_certs(),
            rtt,
            transports,
        }
    }
}

#[cfg(test)]
mod tests {
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::{ClientCapabilities, PROTOCOL_VERSION};

    use crate::core::AppCore;

    #[test]
    fn connection_info_reports_protocol_and_transport() {
        let mgr = SessionManager::new(
            "10.0.0.2".into(),
            8443,
            "tok".into(),
            false,
            ClientCapabilities::default(),
        );
        let core = AppCore::for_test(mgr);
        let info = core.connection_info();
        // Protocol version is always the compiled-in wire version.
        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        // Default transport before any swap is QUIC.
        assert_eq!(info.transport, "QUIC");
        // No Ping/Pong yet → no RTT summary, no per-transport traffic.
        assert!(info.rtt.is_none());
        assert!(info.transports.is_empty());
    }
}
