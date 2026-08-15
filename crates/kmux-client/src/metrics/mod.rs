//! Client-side metrics subsystem.
//!
//! [`MetricsStore`] is the single aggregate the `SessionManager` holds. It
//! owns four focused collectors:
//!
//! - [`render::RenderMetrics`] — frame-apply latency, batch size, diag
//!   counters (pre-existing; unchanged).
//! - [`network::NetworkMetrics`] — per-(transport, category) byte/msg counts.
//! - [`rtt::RttTracker`] — Ping/Pong round-trip samples per transport.
//! - [`jsonl::JsonlSink`] — optional rolling persistence so multiple
//!   concurrent `kmux` processes can share a metrics log.
//!
//! See `docs/metrics.md` for the full architecture and JSONL schema.

pub mod jsonl;
pub mod network;
mod render;
pub mod rtt;

pub use jsonl::{JsonlSink, Sample};
pub use network::{NetworkMetrics, TransportCounters, TransportKey};
pub use render::{MetricsSnapshot, RenderMetrics, RingBuffer};
pub use rtt::{RttSummary, RttTracker};

use kmux_protocol::messages::{ConnectionId, MessageCategory, TransportKind, epoch_millis};

use crate::event_log::DiagSnapshot;

/// Single facade over the four metrics collectors. Existing `record_*`
/// methods delegate to [`RenderMetrics`] so the `SessionManager` diff is
/// minimal; the new collectors are exposed through typed fields.
pub struct MetricsStore {
    render: RenderMetrics,
    pub network: NetworkMetrics,
    pub rtt: RttTracker,
    sink: Option<JsonlSink>,
}

impl MetricsStore {
    /// Build a store with persistence enabled. `sink_path` is typically
    /// `kmux_sys::dirs::metrics_log_path()`; pass `None` in tests
    /// or when disk writes are unwelcome.
    pub fn new(sink: Option<JsonlSink>) -> Self {
        Self {
            render: RenderMetrics::new(),
            network: NetworkMetrics::new(),
            rtt: RttTracker::new(),
            sink,
        }
    }

    /// In-process-only store (no JSONL persistence). Used by tests and
    /// anywhere we don't want to touch the filesystem.
    pub fn in_memory() -> Self {
        Self::new(None)
    }

    // ── Transport tagging ────────────────────────────────────────────────

    /// Called when a new data-plane channel becomes active (initial attach
    /// or after `ChannelSwitched`). All subsequent byte/msg counters
    /// attribute to this transport until the next call.
    pub fn on_transport_active(&mut self, kind: TransportKind, address: impl Into<String>) {
        self.network.set_active(TransportKey::new(kind, address));
    }

    pub fn active_transport(&self) -> Option<&TransportKey> {
        self.network.current()
    }

    // ── RTT observation ──────────────────────────────────────────────────

    /// Record an RTT measurement against the currently active transport.
    /// No-op if no transport is tagged yet (e.g. before first attach).
    pub fn observe_rtt(&mut self, rtt_ms: f64) {
        if let Some(key) = self.network.current().cloned() {
            self.rtt.observe(&key, rtt_ms);
        }
    }

    // ── Byte/message counting ────────────────────────────────────────────

    pub fn record_outbound(&mut self, bytes: usize, category: MessageCategory) {
        self.network.record_outbound(bytes, category);
    }

    pub fn record_inbound(&mut self, bytes: usize, category: MessageCategory) {
        self.network.record_inbound(bytes, category);
    }

    // ── RenderMetrics delegations ────────────────────────────────────────
    // These exist so existing call sites in `SessionManager` continue to
    // compile unchanged.

    pub fn record_apply(&mut self, sent_at_ms: u64, apply_elapsed_ms: f64) {
        self.render.record_apply(sent_at_ms, apply_elapsed_ms);
    }

    pub fn record_batch(&mut self, size: usize) {
        self.render.record_batch(size);
    }

    pub fn record_diff_stats(&mut self, ops: usize) {
        self.render.record_diff_stats(ops);
    }

    pub fn record_large_diff(&mut self, net_apply_ms: f64) {
        self.render.record_large_diff(net_apply_ms);
    }

    pub fn record_stale_discard(&mut self, session: &str) {
        self.render.record_stale_discard(session);
    }

    pub fn record_seqno_gap(&mut self, session: &str, expected: u64, got: u64) {
        self.render.record_seqno_gap(session, expected, got);
    }

    pub fn record_lag(&mut self, session: &str, missed: u64) {
        self.render.record_lag(session, missed);
    }

    pub fn record_resync(&mut self, session: &str, reason: &str) {
        self.render.record_resync(session, reason);
    }

    pub fn record_tear(&mut self, session: &str, prev_sent_at_ms: u64, next_sent_at_ms: u64) {
        self.render
            .record_tear(session, prev_sent_at_ms, next_sent_at_ms);
    }

    pub fn record_digest_mismatch(&mut self, session: &str, seqno: u64) {
        self.render.record_digest_mismatch(session, seqno);
    }

    /// Count of grid-digest mismatches recorded so far (for diagnostics/tests).
    pub fn digest_mismatch_count(&self) -> u64 {
        self.render.diag_counters().digest_mismatches
    }

    pub fn snapshot(&self, snapshot_mode: bool) -> MetricsSnapshot {
        self.render.snapshot(snapshot_mode)
    }

    pub fn diag_snapshot(&self) -> DiagSnapshot {
        self.render.diag_snapshot()
    }

    // ── Persistence ──────────────────────────────────────────────────────

    /// Append one sample per active `(transport, category)` bucket that has
    /// non-zero delta since the previous flush. RTT and render fields are
    /// replicated on every row (they describe the transport, not the category).
    /// Nothing is written when no traffic moved since the last flush.
    pub fn flush_sample(&mut self, conn_id: Option<ConnectionId>) {
        let Some(sink) = &self.sink else { return };
        let deltas = self.network.take_deltas_for_active();
        if deltas.is_empty() {
            return;
        }

        let ts_ms = epoch_millis();
        let net_apply_avg = self.render.net_apply_avg();
        let net_apply_max = self.render.net_apply_max();

        for (transport_key, category, counters) in &deltas {
            let rtt = self.rtt.summary(transport_key);
            let sample = Sample::from_bucket(
                ts_ms,
                conn_id.map(|c| c.0),
                transport_key,
                &category.to_string(),
                *counters,
                rtt,
                (net_apply_avg, net_apply_max),
            );
            sink.append(&sample);
        }
    }

    /// Path where samples are persisted, if a sink is configured.
    pub fn sink_path(&self) -> Option<&std::path::Path> {
        self.sink.as_ref().map(JsonlSink::path)
    }
}

impl Default for MetricsStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_attributes_to_active_transport_and_category() {
        let mut s = MetricsStore::in_memory();
        s.on_transport_active(TransportKind::Uds, "/run/sock");
        s.record_outbound(128, MessageCategory::Shell);
        s.record_inbound(256, MessageCategory::Shell);
        let snap = s.network.snapshot();
        assert_eq!(snap.len(), 1);
        let (_, cat, c) = &snap[0];
        assert_eq!(*cat, MessageCategory::Shell);
        assert_eq!(c.bytes_out, 128);
        assert_eq!(c.bytes_in, 256);
    }

    #[test]
    fn multiple_categories_produce_separate_buckets() {
        let mut s = MetricsStore::in_memory();
        s.on_transport_active(TransportKind::Uds, "/run/sock");
        s.record_outbound(10, MessageCategory::Shell);
        s.record_outbound(5, MessageCategory::Liveness);
        let snap = s.network.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn observe_rtt_requires_active_transport() {
        let mut s = MetricsStore::in_memory();
        // Before attach — swallowed.
        s.observe_rtt(15.0);
        // After attach — recorded.
        s.on_transport_active(TransportKind::Quic, "1.2.3.4:8443");
        s.observe_rtt(15.0);
        let key = TransportKey::new(TransportKind::Quic, "1.2.3.4:8443");
        assert_eq!(s.rtt.summary(&key).unwrap().sample_count, 1);
    }

    #[test]
    fn flush_sample_without_sink_is_noop() {
        let mut s = MetricsStore::in_memory();
        s.on_transport_active(TransportKind::Uds, "/x");
        s.record_outbound(10, MessageCategory::Shell);
        // Should not panic or block.
        s.flush_sample(Some(ConnectionId(7)));
    }

    #[test]
    fn flush_sample_noop_when_no_traffic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.jsonl");
        let mut s = MetricsStore::new(Some(JsonlSink::new(path.clone())));
        s.on_transport_active(TransportKind::Uds, "/x");
        // No records — flush should not write anything.
        s.flush_sample(Some(ConnectionId(1)));
        assert!(!path.exists());
    }

    #[test]
    fn flush_sample_emits_one_row_per_category() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.jsonl");
        let mut s = MetricsStore::new(Some(JsonlSink::new(path.clone())));
        s.on_transport_active(TransportKind::Uds, "/x");
        s.record_outbound(100, MessageCategory::Shell);
        s.record_inbound(50, MessageCategory::Liveness);
        s.flush_sample(Some(ConnectionId(1)));

        let sink = JsonlSink::new(path);
        let rows = sink.read_history(100).unwrap();
        assert_eq!(rows.len(), 2);
        let cats: Vec<_> = rows.iter().map(|r| r.category.as_deref()).collect();
        assert!(cats.contains(&Some("Shell")));
        assert!(cats.contains(&Some("Liveness")));
    }

    #[test]
    fn render_delegations_reach_inner_metrics() {
        let mut s = MetricsStore::in_memory();
        s.record_stale_discard("pane-1");
        s.record_stale_discard("pane-1");
        assert_eq!(s.snapshot(false).counters.stale_discards, 2);
    }

    #[test]
    fn record_tear_increments_counter() {
        let mut s = MetricsStore::in_memory();
        s.record_tear("pane-1", 1000, 1008);
        s.record_tear("pane-1", 1008, 1015);
        assert_eq!(s.snapshot(false).counters.tears, 2);
    }
}
