//! Client-side rendering metrics: network+apply latency, apply duration,
//! batch size, diff stats, and diagnostic counters. Pre-existing module
//! moved verbatim out of `metrics.rs` when the metrics subsystem grew
//! additional collectors (`network`, `rtt`, `jsonl`).

use std::collections::VecDeque;

use kmux_protocol::messages::epoch_millis;

use crate::event_log::{DiagCounters, DiagEvent, DiagSnapshot, EventLog};

const DEFAULT_CAPACITY: usize = 128;

/// Fixed-capacity ring buffer of `f64` samples.
pub struct RingBuffer {
    data: VecDeque<f64>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.data.len() == self.capacity {
            self.data.pop_front();
        }
        self.data.push_back(value);
    }

    pub fn avg(&self) -> f64 {
        if self.data.is_empty() {
            return 0.0;
        }
        self.data.iter().sum::<f64>() / self.data.len() as f64
    }

    pub fn max(&self) -> f64 {
        self.data.iter().cloned().fold(0.0_f64, f64::max)
    }

    #[cfg(test)]
    pub fn last(&self) -> f64 {
        self.data.back().cloned().unwrap_or(0.0)
    }

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

/// Snapshot of metrics for the HUD overlay. Cheap to copy.
#[derive(Clone, Copy)]
pub struct MetricsSnapshot {
    pub net_apply_avg_ms: f64,
    pub net_apply_max_ms: f64,
    pub apply_avg_ms: f64,
    pub batch_avg: f64,
    pub last_diff_ops: usize,
    pub last_large_diff_ms: f64,
    pub counters: DiagCounters,
    pub snapshot_mode: bool,
}

/// Collects client-side timing metrics.
pub struct RenderMetrics {
    network_apply_latency: RingBuffer,
    apply_duration: RingBuffer,
    batch_size: RingBuffer,
    last_diff_ops: usize,
    /// End-to-end latency (sent_at → apply complete) for the most recent large diff (>100 ops).
    last_large_diff_ms: f64,
    counters: DiagCounters,
    event_log: EventLog,
}

impl RenderMetrics {
    pub fn new() -> Self {
        Self {
            network_apply_latency: RingBuffer::new(DEFAULT_CAPACITY),
            apply_duration: RingBuffer::new(DEFAULT_CAPACITY),
            batch_size: RingBuffer::new(DEFAULT_CAPACITY),
            last_diff_ops: 0,
            last_large_diff_ms: 0.0,
            counters: DiagCounters::default(),
            event_log: EventLog::new(),
        }
    }

    /// Record a single message application.
    /// `sent_at_ms`: the server's `epoch_millis()` timestamp on the message.
    /// `apply_elapsed_ms`: wall-clock time spent in `apply_diff`/`apply_snapshot`.
    pub fn record_apply(&mut self, sent_at_ms: u64, apply_elapsed_ms: f64) {
        let now = epoch_millis();
        let net_apply = now.saturating_sub(sent_at_ms) as f64;
        self.network_apply_latency.push(net_apply);
        self.apply_duration.push(apply_elapsed_ms);
    }

    /// Record the size of an incoming `ServerMsgBatch`.
    pub fn record_batch(&mut self, size: usize) {
        self.batch_size.push(size as f64);
    }

    /// Record cell diff statistics for HUD display.
    pub fn record_diff_stats(&mut self, ops: usize) {
        self.last_diff_ops = ops;
    }

    /// Record a large diff event (>100 ops) with its end-to-end latency.
    pub fn record_large_diff(&mut self, net_apply_ms: f64) {
        self.last_large_diff_ms = net_apply_ms;
    }

    pub fn record_stale_discard(&mut self, session: &str) {
        let event = DiagEvent::StaleDiscard {
            session: session.to_owned(),
        };
        self.counters.increment(&event);
        self.event_log.push(event);
    }

    pub fn record_seqno_gap(&mut self, session: &str, expected: u64, got: u64) {
        let event = DiagEvent::SeqnoGap {
            session: session.to_owned(),
            expected,
            got,
        };
        self.counters.increment(&event);
        self.event_log.push(event);
    }

    pub fn record_lag(&mut self, session: &str, missed: u64) {
        let event = DiagEvent::Lagged {
            session: session.to_owned(),
            missed,
        };
        self.counters.increment(&event);
        self.event_log.push(event);
    }

    pub fn record_resync(&mut self, session: &str, reason: &str) {
        let event = DiagEvent::Resync {
            session: session.to_owned(),
            reason: reason.to_owned(),
        };
        self.counters.increment(&event);
        self.event_log.push(event);
    }

    /// Record a detected tear: a partial logical frame was painted (issue #72).
    pub fn record_tear(&mut self, session: &str, prev_sent_at_ms: u64, next_sent_at_ms: u64) {
        let event = DiagEvent::Tear {
            session: session.to_owned(),
            prev_sent_at_ms,
            next_sent_at_ms,
        };
        self.counters.increment(&event);
        self.event_log.push(event);
    }

    pub fn snapshot(&self, snapshot_mode: bool) -> MetricsSnapshot {
        MetricsSnapshot {
            net_apply_avg_ms: self.network_apply_latency.avg(),
            net_apply_max_ms: self.network_apply_latency.max(),
            apply_avg_ms: self.apply_duration.avg(),
            batch_avg: self.batch_size.avg(),
            last_diff_ops: self.last_diff_ops,
            last_large_diff_ms: self.last_large_diff_ms,
            counters: self.counters,
            snapshot_mode,
        }
    }

    pub fn diag_snapshot(&self) -> DiagSnapshot {
        DiagSnapshot::from_log(&self.event_log, 8)
    }

    /// Exposed so [`super::MetricsStore::snapshot`] can populate a combined sample
    /// without re-computing averages twice.
    pub(super) fn net_apply_avg(&self) -> f64 {
        self.network_apply_latency.avg()
    }

    pub(super) fn net_apply_max(&self) -> f64 {
        self.network_apply_latency.max()
    }
}

impl Default for RenderMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_empty() {
        let rb = RingBuffer::new(4);
        assert_eq!(rb.avg(), 0.0);
        assert_eq!(rb.max(), 0.0);
        assert_eq!(rb.last(), 0.0);
        assert_eq!(rb.len(), 0);
    }

    #[test]
    fn ring_buffer_basic() {
        let mut rb = RingBuffer::new(4);
        rb.push(10.0);
        rb.push(20.0);
        rb.push(30.0);
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.avg(), 20.0);
        assert_eq!(rb.max(), 30.0);
        assert_eq!(rb.last(), 30.0);
    }

    #[test]
    fn ring_buffer_wrap() {
        let mut rb = RingBuffer::new(3);
        rb.push(1.0);
        rb.push(2.0);
        rb.push(3.0);
        rb.push(4.0); // evicts 1.0
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.avg(), 3.0); // (2+3+4)/3
        assert_eq!(rb.max(), 4.0);
    }

    #[test]
    fn record_apply() {
        let mut m = RenderMetrics::new();
        // Simulate: server sent 10ms ago, apply took 0.5ms
        let sent_at = epoch_millis() - 10;
        m.record_apply(sent_at, 0.5);
        let snap = m.snapshot(false);
        assert!(snap.net_apply_avg_ms >= 9.0); // at least ~10ms of simulated latency
        assert!((snap.apply_avg_ms - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn record_batch() {
        let mut m = RenderMetrics::new();
        m.record_batch(5);
        m.record_batch(3);
        let snap = m.snapshot(false);
        assert_eq!(snap.batch_avg, 4.0);
    }
}
