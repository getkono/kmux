use std::fmt;

/// The active transport channel between client and daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// QUIC/UDP transport (preferred; lower latency, multiplexed streams).
    Quic,
    /// TCP transport (fallback; tunnels through SSH or other TCP proxies).
    Tcp,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportKind::Quic => write!(f, "QUIC"),
            TransportKind::Tcp => write!(f, "TCP"),
        }
    }
}
