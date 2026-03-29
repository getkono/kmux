use std::collections::VecDeque;

use smux_protocol::messages::epoch_millis;

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
    pub last_dirty_rows: usize,
}

/// Collects client-side timing metrics.
pub struct RenderMetrics {
    network_apply_latency: RingBuffer,
    apply_duration: RingBuffer,
    batch_size: RingBuffer,
    last_diff_ops: usize,
    last_dirty_rows: usize,
}

impl RenderMetrics {
    pub fn new() -> Self {
        Self {
            network_apply_latency: RingBuffer::new(DEFAULT_CAPACITY),
            apply_duration: RingBuffer::new(DEFAULT_CAPACITY),
            batch_size: RingBuffer::new(DEFAULT_CAPACITY),
            last_diff_ops: 0,
            last_dirty_rows: 0,
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
    pub fn record_diff_stats(&mut self, ops: usize, dirty_rows: usize) {
        self.last_diff_ops = ops;
        self.last_dirty_rows = dirty_rows;
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            net_apply_avg_ms: self.network_apply_latency.avg(),
            net_apply_max_ms: self.network_apply_latency.max(),
            apply_avg_ms: self.apply_duration.avg(),
            batch_avg: self.batch_size.avg(),
            last_diff_ops: self.last_diff_ops,
            last_dirty_rows: self.last_dirty_rows,
        }
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
        let snap = m.snapshot();
        assert!(snap.net_apply_avg_ms >= 9.0); // at least ~10ms of simulated latency
        assert!((snap.apply_avg_ms - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn record_batch() {
        let mut m = RenderMetrics::new();
        m.record_batch(5);
        m.record_batch(3);
        let snap = m.snapshot();
        assert_eq!(snap.batch_avg, 4.0);
    }
}
