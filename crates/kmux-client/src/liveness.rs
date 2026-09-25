//! Bidirectional ping liveness tracker.
//!
//! The server sends `ServerMessage::Ping` every [`PING_INTERVAL`]. This
//! module:
//!
//! 1. Notes that every received frame (not just `Pong`) counts as proof of
//!    life — the session's data plane is healthy as long as *something*
//!    is coming back from the server.
//! 2. Drives the client's own outbound `ClientMessage::Ping` on the same
//!    cadence, which lets us detect asymmetric path failures (a QUIC path
//!    that can still deliver bytes server→client but silently drops the
//!    reverse direction).
//! 3. Declares `is_timed_out()` true when no inbound frame has arrived
//!    for longer than [`TIMEOUT`].
//!
//! The tracker itself is pure — it takes an injected `now: Instant` so
//! tests do not need tokio's time paused. The event loop calls
//! `Liveness::tick` every render cycle and pulls any outbound ping via
//! [`Liveness::client_ping_due`].

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use kmux_protocol::messages::ClientMessage;

pub const PING_INTERVAL: Duration = Duration::from_secs(5);
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// Tracker state. Reset on every new connection via [`Liveness::reset`].
#[derive(Debug)]
pub struct Liveness {
    last_inbound: Instant,
    next_client_ping: Instant,
    outstanding: BTreeMap<u64, Instant>,
    next_seq: u64,
    ping_interval: Duration,
    timeout: Duration,
}

impl Liveness {
    pub fn new(now: Instant) -> Self {
        Self::with_config(now, PING_INTERVAL, TIMEOUT)
    }

    pub fn with_config(now: Instant, ping_interval: Duration, timeout: Duration) -> Self {
        Self {
            last_inbound: now,
            next_client_ping: now + ping_interval,
            outstanding: BTreeMap::new(),
            next_seq: 0,
            ping_interval,
            timeout,
        }
    }

    /// Called when the `SessionManager` hands a new sender over after a
    /// successful reconnect or transport upgrade.
    pub fn reset(&mut self, now: Instant) {
        self.last_inbound = now;
        self.next_client_ping = now + self.ping_interval;
        self.outstanding.clear();
    }

    /// Record that a server frame (any kind) was just decoded. This is the
    /// single authoritative "the server is alive" signal.
    pub fn observe_inbound(&mut self, now: Instant) {
        self.last_inbound = now;
    }

    /// Record a `ServerMessage::Pong { seq }` we received for one of our
    /// own pings. Drops the matching outstanding entry (and any older ones
    /// — if seq N came back, N-1 is also effectively alive). Returns the
    /// RTT observed on the matched entry, if any, so the caller can feed
    /// it to the metrics/supervisor layer.
    pub fn on_pong(&mut self, seq: u64, now: Instant) -> Option<Duration> {
        self.last_inbound = now;
        let sent_at = self.outstanding.get(&seq).copied();
        self.outstanding.retain(|&k, _| k > seq);
        sent_at.map(|t| now.saturating_duration_since(t))
    }

    /// If the outbound ping cadence has elapsed, return the ping message
    /// to send and advance the schedule. The caller is responsible for
    /// putting the message on the wire.
    pub fn client_ping_due(&mut self, now: Instant) -> Option<ClientMessage> {
        if now < self.next_client_ping {
            return None;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.outstanding.insert(seq, now);
        self.next_client_ping = now + self.ping_interval;
        Some(ClientMessage::Ping { seq })
    }

    /// True if no inbound frame (including Pongs) has arrived within
    /// the timeout window.
    pub fn is_timed_out(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_inbound) > self.timeout
    }

    /// How long since the last inbound frame of any kind (issue #61: the HUD's
    /// latency counter stars when this exceeds 3× the ping interval).
    pub fn idle_since(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_inbound)
    }

    /// Next instant at which the event loop must wake up to either send
    /// a ping or re-evaluate the timeout. Used to arm a `sleep_until`.
    pub fn next_wakeup(&self) -> Instant {
        let deadline = self.last_inbound + self.timeout;
        self.next_client_ping.min(deadline)
    }

    #[cfg(test)]
    pub fn outstanding_count(&self) -> usize {
        self.outstanding.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn inbound_keeps_session_alive_past_timeout() {
        let start = t0();
        let mut l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));

        // 90s elapsed but we keep observing inbound frames every 20s.
        for i in 1..=5 {
            let now = start + Duration::from_secs(i * 20);
            l.observe_inbound(now);
            assert!(!l.is_timed_out(now), "should be alive at t={}", i * 20);
        }
    }

    #[test]
    fn timeout_triggers_when_no_inbound() {
        let start = t0();
        let l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));
        assert!(!l.is_timed_out(start + Duration::from_secs(59)));
        assert!(l.is_timed_out(start + Duration::from_secs(61)));
    }

    #[test]
    fn client_ping_emits_sequential_seqs_on_interval() {
        let start = t0();
        let mut l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));

        // No ping due immediately.
        assert!(l.client_ping_due(start).is_none());

        let p1 = l.client_ping_due(start + Duration::from_secs(30)).unwrap();
        assert!(matches!(p1, ClientMessage::Ping { seq: 0 }));

        // Not due again for another 30s.
        assert!(l.client_ping_due(start + Duration::from_secs(45)).is_none());

        let p2 = l.client_ping_due(start + Duration::from_secs(60)).unwrap();
        assert!(matches!(p2, ClientMessage::Ping { seq: 1 }));
    }

    #[test]
    fn pong_acknowledges_outstanding_pings_and_refreshes_liveness() {
        let start = t0();
        let mut l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));

        // Issue two pings.
        l.client_ping_due(start + Duration::from_secs(30));
        l.client_ping_due(start + Duration::from_secs(60));
        assert_eq!(l.outstanding_count(), 2);

        // Pong for the later seq drops both (older ones are implicitly acked).
        let rtt = l.on_pong(1, start + Duration::from_secs(61));
        assert_eq!(l.outstanding_count(), 0);
        // seq=1 was issued at +60s, Pong arrived at +61s → 1s RTT.
        assert_eq!(rtt, Some(Duration::from_secs(1)));

        // A Pong for a seq we never issued should return None without
        // touching outstanding.
        assert!(l.on_pong(99, start + Duration::from_secs(62)).is_none());

        // Inbound was refreshed — 59s later we are still alive.
        assert!(!l.is_timed_out(start + Duration::from_secs(119)));
    }

    #[test]
    fn reset_restarts_schedule() {
        let start = t0();
        let mut l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));
        l.client_ping_due(start + Duration::from_secs(30));
        assert_eq!(l.outstanding_count(), 1);

        let reconnect = start + Duration::from_secs(120);
        l.reset(reconnect);
        assert_eq!(l.outstanding_count(), 0);
        // Fresh schedule: no ping due until reconnect + 30s.
        assert!(l.client_ping_due(reconnect).is_none());
        assert!(
            l.client_ping_due(reconnect + Duration::from_secs(30))
                .is_some()
        );
        // And timeout is measured from reconnect, not the original start.
        assert!(!l.is_timed_out(reconnect + Duration::from_secs(59)));
    }

    #[test]
    fn next_wakeup_is_min_of_ping_and_timeout_deadlines() {
        let start = t0();
        let l = Liveness::with_config(start, Duration::from_secs(30), Duration::from_secs(60));
        // Fresh: next_client_ping = +30s, timeout deadline = +60s; min = +30s.
        assert_eq!(l.next_wakeup(), start + Duration::from_secs(30));
    }
}
