//! Per-transport, per-category byte and message counters.
//!
//! `NetworkMetrics` tracks cumulative bytes and messages for each distinct
//! `(TransportKey, MessageCategory)` pair the client has observed during the
//! session. An entry is created lazily the first time traffic of that category
//! flows over a given transport.
//!
//! The sampler ([`crate::metrics::jsonl::JsonlSink`]) emits *delta* samples —
//! the difference since the previous flush — so concurrent `kmux` processes
//! don't double-count.

use std::collections::HashMap;

use kmux_protocol::messages::{MessageCategory, TransportKind};

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

/// Cumulative counters for a single (transport, category) bucket.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransportCounters {
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
}

impl TransportCounters {
    pub fn is_zero(self) -> bool {
        self.bytes_in == 0 && self.bytes_out == 0 && self.msgs_in == 0 && self.msgs_out == 0
    }
}

/// Per-(transport, category) counters; the current active transport is tracked
/// so callers don't have to pass the key on every increment.
pub struct NetworkMetrics {
    per_bucket: HashMap<(TransportKey, MessageCategory), TransportCounters>,
    /// Counters at the last call to [`Self::take_deltas_for_active`] — used to
    /// compute deltas for the JSONL sampler.
    previous: HashMap<(TransportKey, MessageCategory), TransportCounters>,
    current: Option<TransportKey>,
}

impl NetworkMetrics {
    pub fn new() -> Self {
        Self {
            per_bucket: HashMap::new(),
            previous: HashMap::new(),
            current: None,
        }
    }

    /// Tag subsequent `record_*` calls with this transport. Idempotent.
    pub fn set_active(&mut self, key: TransportKey) {
        self.current = Some(key);
    }

    /// The currently active `(kind, address)` pair, if any.
    pub fn current(&self) -> Option<&TransportKey> {
        self.current.as_ref()
    }

    fn record(&mut self, bytes: u64, outbound: bool, category: MessageCategory) {
        let Some(key) = self.current.clone() else {
            return;
        };
        let entry = self.per_bucket.entry((key, category)).or_default();
        if outbound {
            entry.bytes_out += bytes;
            entry.msgs_out += 1;
        } else {
            entry.bytes_in += bytes;
            entry.msgs_in += 1;
        }
    }

    pub fn record_outbound(&mut self, bytes: usize, category: MessageCategory) {
        self.record(bytes as u64, true, category);
    }

    pub fn record_inbound(&mut self, bytes: usize, category: MessageCategory) {
        self.record(bytes as u64, false, category);
    }

    /// Snapshot of all `(transport, category)` buckets with non-zero counters.
    /// The caller can sort/display without holding a borrow.
    pub fn snapshot(&self) -> Vec<(TransportKey, MessageCategory, TransportCounters)> {
        self.per_bucket
            .iter()
            .filter(|(_, c)| !c.is_zero())
            .map(|((k, cat), c)| (k.clone(), *cat, *c))
            .collect()
    }

    /// Aggregated per-transport totals (summed across all categories). Used by
    /// the overlay transport card header.
    pub fn snapshot_by_transport(&self) -> Vec<(TransportKey, TransportCounters)> {
        let mut totals: HashMap<&TransportKey, TransportCounters> = HashMap::new();
        for ((k, _), c) in &self.per_bucket {
            let t = totals.entry(k).or_default();
            t.bytes_in += c.bytes_in;
            t.bytes_out += c.bytes_out;
            t.msgs_in += c.msgs_in;
            t.msgs_out += c.msgs_out;
        }
        totals.into_iter().map(|(k, c)| (k.clone(), c)).collect()
    }

    /// Return per-category deltas for the currently active transport since the
    /// previous call to this method. Only buckets with a non-zero delta are
    /// returned. Used by the JSONL sampler so two concurrent clients don't each
    /// write full cumulative counts.
    pub fn take_deltas_for_active(
        &mut self,
    ) -> Vec<(TransportKey, MessageCategory, TransportCounters)> {
        let Some(key) = self.current.clone() else {
            return Vec::new();
        };

        let mut deltas = Vec::new();
        for cat in MessageCategory::all() {
            let bucket_key = (key.clone(), *cat);
            let current = self
                .per_bucket
                .get(&bucket_key)
                .copied()
                .unwrap_or_default();
            let prev = self.previous.get(&bucket_key).copied().unwrap_or_default();
            let delta = TransportCounters {
                bytes_in: current.bytes_in - prev.bytes_in,
                bytes_out: current.bytes_out - prev.bytes_out,
                msgs_in: current.msgs_in - prev.msgs_in,
                msgs_out: current.msgs_out - prev.msgs_out,
            };
            self.previous.insert(bucket_key, current);
            if !delta.is_zero() {
                deltas.push((key.clone(), *cat, delta));
            }
        }
        deltas
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

    fn quic(addr: &str) -> TransportKey {
        TransportKey::new(TransportKind::Quic, addr)
    }

    #[test]
    fn records_attribute_to_current_active_and_category() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/run/kmux/daemon-data.sock"));
        m.record_outbound(100, MessageCategory::Shell);
        m.record_inbound(200, MessageCategory::Shell);
        m.record_outbound(50, MessageCategory::Liveness);

        let snap = m.snapshot();
        let shell = snap
            .iter()
            .find(|(_, c, _)| *c == MessageCategory::Shell)
            .unwrap();
        let liveness = snap
            .iter()
            .find(|(_, c, _)| *c == MessageCategory::Liveness)
            .unwrap();
        assert_eq!(shell.2.bytes_out, 100);
        assert_eq!(shell.2.bytes_in, 200);
        assert_eq!(shell.2.msgs_out, 1);
        assert_eq!(liveness.2.bytes_out, 50);
        assert_eq!(liveness.2.bytes_in, 0);
    }

    #[test]
    fn switching_transports_preserves_history() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100, MessageCategory::Shell);

        m.set_active(quic("1.2.3.4:8443"));
        m.record_outbound(500, MessageCategory::Shell);
        m.record_inbound(700, MessageCategory::Shell);

        let by_t = m.snapshot_by_transport();
        let quic_totals = by_t
            .iter()
            .find(|(k, _)| k.kind == TransportKind::Quic)
            .unwrap();
        let uds_totals = by_t
            .iter()
            .find(|(k, _)| k.kind == TransportKind::Uds)
            .unwrap();
        assert_eq!(quic_totals.1.bytes_out, 500);
        assert_eq!(quic_totals.1.bytes_in, 700);
        assert_eq!(uds_totals.1.bytes_out, 100);
    }

    #[test]
    fn record_without_active_is_noop() {
        let mut m = NetworkMetrics::new();
        m.record_outbound(42, MessageCategory::Control);
        m.record_inbound(99, MessageCategory::Shell);
        assert!(m.snapshot().is_empty());
    }

    #[test]
    fn delta_returns_only_nonzero_categories() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100, MessageCategory::Shell);
        m.record_inbound(200, MessageCategory::Shell);

        let d1 = m.take_deltas_for_active();
        assert_eq!(d1.len(), 1);
        let (_, cat, c) = &d1[0];
        assert_eq!(*cat, MessageCategory::Shell);
        assert_eq!(c.bytes_out, 100);
        assert_eq!(c.bytes_in, 200);

        // Second flush: only the new outbound liveness tick.
        m.record_outbound(25, MessageCategory::Liveness);
        let d2 = m.take_deltas_for_active();
        assert_eq!(d2.len(), 1);
        assert_eq!(d2[0].1, MessageCategory::Liveness);
        assert_eq!(d2[0].2.bytes_out, 25);
    }

    #[test]
    fn delta_without_active_returns_empty() {
        let mut m = NetworkMetrics::new();
        assert!(m.take_deltas_for_active().is_empty());
    }

    #[test]
    fn delta_preserves_per_category_previous_state_across_transport_switch() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100, MessageCategory::Shell);
        let _ = m.take_deltas_for_active();

        // Switch transport; the UDS previous state must not contaminate QUIC deltas.
        m.set_active(quic("1.2.3.4:8443"));
        m.record_outbound(50, MessageCategory::Shell);
        let d = m.take_deltas_for_active();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].2.bytes_out, 50);
    }

    #[test]
    fn snapshot_by_transport_sums_across_categories() {
        let mut m = NetworkMetrics::new();
        m.set_active(uds("/a"));
        m.record_outbound(100, MessageCategory::Shell);
        m.record_outbound(20, MessageCategory::Liveness);
        m.record_inbound(50, MessageCategory::Control);

        let by_t = m.snapshot_by_transport();
        assert_eq!(by_t.len(), 1);
        assert_eq!(by_t[0].1.bytes_out, 120);
        assert_eq!(by_t[0].1.bytes_in, 50);
        assert_eq!(by_t[0].1.msgs_out, 2);
        assert_eq!(by_t[0].1.msgs_in, 1);
    }
}
