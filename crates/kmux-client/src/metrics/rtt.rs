//! Per-transport RTT tracker fed by Ping/Pong measurements.
//!
//! The seq→send-time map already lives in [`crate::liveness::Liveness`]; this
//! module is deliberately thin and only keeps the rolling history needed to
//! render averages / maxes in the metrics overlay.
//!
//! `SessionManager` pushes observations in via [`RttTracker::observe`] after
//! [`crate::liveness::Liveness::on_pong`] hands back the RTT for the matched
//! seq. The same value is also forwarded to
//! [`crate::supervisor::EndpointHealth::record_rtt`] so the transport scorer
//! sees real measurements instead of the `LATENCY_UNKNOWN_MS` default.

use std::collections::HashMap;

use super::network::TransportKey;
use super::render::RingBuffer;

const RTT_CAPACITY: usize = 64;
const EWMA_ALPHA: f64 = 0.2;

/// Summary of recent RTT samples for a single transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct RttSummary {
    pub ewma_ms: Option<f64>,
    pub recent_avg_ms: f64,
    pub recent_max_ms: f64,
    pub sample_count: usize,
}

struct History {
    samples: RingBuffer,
    ewma: Option<f64>,
    count: usize,
}

impl History {
    fn new() -> Self {
        Self {
            samples: RingBuffer::new(RTT_CAPACITY),
            ewma: None,
            count: 0,
        }
    }

    fn observe(&mut self, rtt_ms: f64) {
        self.samples.push(rtt_ms);
        self.ewma = Some(match self.ewma {
            Some(prev) => prev * (1.0 - EWMA_ALPHA) + rtt_ms * EWMA_ALPHA,
            None => rtt_ms,
        });
        self.count += 1;
    }

    fn summary(&self) -> RttSummary {
        RttSummary {
            ewma_ms: self.ewma,
            recent_avg_ms: self.samples.avg(),
            recent_max_ms: self.samples.max(),
            sample_count: self.count,
        }
    }
}

pub struct RttTracker {
    per_transport: HashMap<TransportKey, History>,
}

impl RttTracker {
    pub fn new() -> Self {
        Self {
            per_transport: HashMap::new(),
        }
    }

    /// Record an RTT measurement against `transport`. Creates the history
    /// entry lazily.
    pub fn observe(&mut self, transport: &TransportKey, rtt_ms: f64) {
        self.per_transport
            .entry(transport.clone())
            .or_insert_with(History::new)
            .observe(rtt_ms);
    }

    pub fn summary(&self, transport: &TransportKey) -> Option<RttSummary> {
        self.per_transport.get(transport).map(History::summary)
    }

    pub fn all_summaries(&self) -> Vec<(TransportKey, RttSummary)> {
        self.per_transport
            .iter()
            .map(|(k, h)| (k.clone(), h.summary()))
            .collect()
    }
}

impl Default for RttTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use kmux_protocol::messages::TransportKind;

    use super::*;

    fn key() -> TransportKey {
        TransportKey::new(TransportKind::Quic, "10.0.0.1:8443")
    }

    #[test]
    fn observe_populates_summary() {
        let mut t = RttTracker::new();
        t.observe(&key(), 20.0);
        t.observe(&key(), 40.0);
        let s = t.summary(&key()).unwrap();
        assert_eq!(s.sample_count, 2);
        assert_eq!(s.recent_avg_ms, 30.0);
        assert_eq!(s.recent_max_ms, 40.0);
        // ewma: 20, then 20*0.8 + 40*0.2 = 24
        assert!((s.ewma_ms.unwrap() - 24.0).abs() < 0.0001);
    }

    #[test]
    fn summary_missing_transport_returns_none() {
        let t = RttTracker::new();
        assert!(t.summary(&key()).is_none());
    }

    #[test]
    fn multiple_transports_tracked_independently() {
        let mut t = RttTracker::new();
        let q = TransportKey::new(TransportKind::Quic, "a");
        let u = TransportKey::new(TransportKind::Uds, "/b");
        t.observe(&q, 100.0);
        t.observe(&u, 1.0);
        assert_eq!(t.summary(&q).unwrap().recent_avg_ms, 100.0);
        assert_eq!(t.summary(&u).unwrap().recent_avg_ms, 1.0);
    }
}
