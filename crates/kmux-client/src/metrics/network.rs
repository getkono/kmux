//! Per-transport byte and message counters.
//!
//! `NetworkMetrics` tracks cumulative bytes and messages for each distinct
//! transport the client has used during the session, keyed by
//! `(TransportKind, endpoint_address)`. An entry is created lazily the first
//! time traffic flows over that transport.
//!
//! The sampler ([`crate::metrics::jsonl::JsonlSink`]) emits *delta* samples —
//! the difference between the previous flush's snapshot and the current one —
//! so concurrent `kmux` processes don't double-count.

use std::collections::HashMap;

use kmux_protocol::messages::TransportKind;

/// Identifies a single (transport, remote-endpoint) pair. Kept small and
/// `Clone` so it can live alongside each outbound/inbound metric call.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TransportKey {
    pub kind: TransportKind,
    /// Connection address: `"host:port"` for QUIC/TLS-TCP, absolute path for
    /// UDS. Opaque to the metrics subsystem beyond being a map key.
    pub address: String,
}

impl TransportKey {
    pub fn new(kind: TransportKind, address: impl Into<String>) -> Self {
        Self {
            kind,
            address: address.into(),
        }
    }
}

/// Cumulative counters for a single transport since it was first observed.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportCounters {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
}

/// Per-transport counters; the current active transport is tracked so callers
/// don't have to pass the key on every increment.
pub struct NetworkMetrics {
    per_transport: HashMap<TransportKey, TransportCounters>,
    /// Counters at the last call to [`Self::take_delta`] — used to compute
    /// deltas for the JSONL sampler.
    previous: HashMap<TransportKey, TransportCounters>,
    current: Option<TransportKey>,
}

impl NetworkMetrics {
    pub fn new() -> Self {
        Self {
            per_transport: HashMap::new(),
            previous: HashMap::new(),
            current: None,
        }
    }

    /// Tag subsequent `record_*` calls with this transport. Creates the
    /// counters entry if it doesn't exist. Idempotent.
    pub fn set_active(&mut self, key: TransportKey) {
        self.per_transport.entry(key.clone()).or_default();
        self.current = Some(key);
    }

    /// The currently active `(kind, address)` pair, if any.
    pub fn current(&self) -> Option<&TransportKey> {
        self.current.as_ref()
    }

    fn record(&mut self, bytes: u64, outbound: bool) {
        let Some(key) = self.current.clone() else {
            return;
        };
        let entry = self.per_transport.entry(key).or_default();
        if outbound {
            entry.bytes_out += bytes;
            entry.msgs_out += 1;
        } else {
            entry.bytes_in += bytes;
            entry.msgs_in += 1;
        }
    }

    pub fn record_outbound(&mut self, bytes: usize) {
        self.record(bytes as u64, true);
    }

    pub fn record_inbound(&mut self, bytes: usize) {
        self.record(bytes as u64, false);
    }

    /// Snapshot of all transports' cumulative counters. Copies the map so the
    /// caller can sort/display without holding a borrow.
    pub fn snapshot(&self) -> Vec<(TransportKey, TransportCounters)> {
        self.per_transport
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    /// Return the delta for the currently active transport since the previous
    /// call to this method. Used by the JSONL sampler so two concurrent
    /// clients don't each write the full cumulative count.
    pub fn take_delta_for_active(&mut self) -> Option<(TransportKey, TransportCounters)> {
        let key = self.current.clone()?;
        let current = *self.per_transport.get(&key)?;
        let previous = self.previous.get(&key).copied().unwrap_or_default();
        self.previous.insert(key.clone(), current);
        Some((
            key,
            TransportCounters {
                bytes_in: current.bytes_in - previous.bytes_in,
                bytes_out: current.bytes_out - previous.bytes_out,
                msgs_in: current.msgs_in - previous.msgs_in,
                msgs_out: current.msgs_out - previous.msgs_out,
            },
        ))
    }
}

impl Default for NetworkMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uds(addr: &str) -> TransportKey {
        TransportKey::new(TransportKind::Uds, addr)
    }

    #[test]
    fn records_attribute_to_current_active() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/run/kmux/daemon-data.sock"));
        m.record_outbound(100);
        m.record_inbound(200);
        m.record_outbound(50);

        let snap = m.snapshot();
        assert_eq!(snap.len(), 1);
        let (_, counters) = &snap[0];
        assert_eq!(counters.bytes_out, 150);
        assert_eq!(counters.bytes_in, 200);
        assert_eq!(counters.msgs_out, 2);
        assert_eq!(counters.msgs_in, 1);
    }

    #[test]
    fn switching_transports_preserves_history() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100);

        m.set_active(TransportKey::new(TransportKind::Quic, "1.2.3.4:8443"));
        m.record_outbound(500);
        m.record_inbound(700);

        let snap = m.snapshot();
        assert_eq!(snap.len(), 2);
        let quic = snap
            .iter()
            .find(|(k, _)| k.kind == TransportKind::Quic)
            .unwrap();
        let uds = snap
            .iter()
            .find(|(k, _)| k.kind == TransportKind::Uds)
            .unwrap();
        assert_eq!(quic.1.bytes_out, 500);
        assert_eq!(quic.1.bytes_in, 700);
        assert_eq!(uds.1.bytes_out, 100);
    }

    #[test]
    fn record_without_active_is_noop() {
        let mut m = NetworkMetrics::new();
        m.record_outbound(42);
        m.record_inbound(99);
        assert!(m.snapshot().is_empty());
    }

    #[test]
    fn delta_returns_difference_between_successive_calls() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100);
        m.record_inbound(200);

        let (_, d1) = m.take_delta_for_active().unwrap();
        assert_eq!(d1.bytes_out, 100);
        assert_eq!(d1.bytes_in, 200);

        m.record_outbound(25);
        let (_, d2) = m.take_delta_for_active().unwrap();
        assert_eq!(d2.bytes_out, 25);
        assert_eq!(d2.bytes_in, 0);
    }

    #[test]
    fn delta_without_active_returns_none() {
        let mut m = NetworkMetrics::new();
        assert!(m.take_delta_for_active().is_none());
    }
}
