//! Inbound deadlines for one connection (issue #206): an unauthenticated
//! socket must finish `Auth` in time, and an authenticated one must answer the
//! daemon's pings. Before these, an idle unauthenticated socket held its slot
//! and tasks forever, and a black-holed client was never dropped.
//!
//! The decisions are pure functions of instants ([`auth_verdict`],
//! [`pong_verdict`]); [`Liveness`] records the instants and [`watchdog`] asks
//! it once per [`WATCHDOG_TICK`], closing the connection on the first `Close`.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tracing::warn;

use crate::lock::lock_or_recover;
use crate::outbound::{CloseReason, Closer};

/// How often the daemon pings an authenticated client.
pub(crate) const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long an unauthenticated connection may take to complete `Auth` and
/// `AuthProof`. A client does both on connect, so this is spent only by a
/// socket that sends nothing (or garbage).
pub(crate) const AUTH_DEADLINE: Duration = Duration::from_secs(30);

/// How long after a ping the daemon waits for any inbound frame (a `Pong`, or
/// anything else) before it closes the connection. Six ping intervals: a live
/// client also pings on its own every five seconds, so only a peer that has
/// gone silent in both directions reaches it.
pub(crate) const PONG_DEADLINE: Duration = Duration::from_secs(30);

/// How often the watchdog checks the deadlines; the resolution of both.
const WATCHDOG_TICK: Duration = Duration::from_secs(1);

/// Whether a connection may stay open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Keep,
    Close(CloseReason),
}

/// Keep a connection that authenticated, or that connected no more than
/// `deadline` ago.
pub(crate) fn auth_verdict(
    connected_at: Instant,
    authenticated: bool,
    now: Instant,
    deadline: Duration,
) -> Verdict {
    if authenticated || now.saturating_duration_since(connected_at) <= deadline {
        Verdict::Keep
    } else {
        Verdict::Close(CloseReason::AuthDeadline)
    }
}

/// Keep a connection with no unanswered ping, or whose oldest unanswered ping
/// went out no more than `deadline` ago. A ping is answered by any inbound
/// frame, not only its `Pong`.
pub(crate) fn pong_verdict(
    unanswered_since: Option<Instant>,
    now: Instant,
    deadline: Duration,
) -> Verdict {
    match unanswered_since {
        Some(sent) if now.saturating_duration_since(sent) > deadline => {
            Verdict::Close(CloseReason::PongDeadline)
        }
        _ => Verdict::Keep,
    }
}

/// The instants the deadlines are measured from, updated by the read loop and
/// the ping task.
pub(crate) struct Liveness {
    state: Mutex<State>,
}

struct State {
    connected_at: Instant,
    authenticated: bool,
    /// When the oldest ping sent since the last inbound frame went out.
    unanswered_since: Option<Instant>,
}

impl Liveness {
    pub(crate) fn new(connected_at: Instant) -> Self {
        Self {
            state: Mutex::new(State {
                connected_at,
                authenticated: false,
                unanswered_since: None,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        lock_or_recover(&self.state, "connection liveness")
    }

    /// A frame arrived: every ping so far is answered.
    pub(crate) fn on_inbound(&self) {
        self.state().unanswered_since = None;
    }

    /// The connection authenticated: the auth deadline no longer applies.
    pub(crate) fn on_authenticated(&self) {
        self.state().authenticated = true;
    }

    /// A ping went out at `now`. Only the oldest unanswered one counts.
    pub(crate) fn on_ping_sent(&self, now: Instant) {
        self.state().unanswered_since.get_or_insert(now);
    }

    /// Whether the connection may stay open at `now`.
    pub(crate) fn verdict(&self, now: Instant) -> Verdict {
        let state = self.state();
        match auth_verdict(state.connected_at, state.authenticated, now, AUTH_DEADLINE) {
            Verdict::Keep => pong_verdict(state.unanswered_since, now, PONG_DEADLINE),
            close => close,
        }
    }
}

/// Check `liveness` every [`WATCHDOG_TICK`] and close the connection through
/// `closer` on the first missed deadline.
pub(crate) async fn watchdog(liveness: std::sync::Arc<Liveness>, closer: Closer) {
    let mut tick = tokio::time::interval(WATCHDOG_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Verdict::Close(reason) = liveness.verdict(Instant::now()) {
            warn!(?reason, "connection missed a deadline");
            closer.close(reason);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn auth_verdict_closes_only_an_unauthenticated_connection_past_the_deadline() {
        let t0 = Instant::now();
        let cases = [
            // (elapsed, authenticated, expected)
            (AUTH_DEADLINE - MS, false, Verdict::Keep),
            (AUTH_DEADLINE, false, Verdict::Keep),
            (
                AUTH_DEADLINE + MS,
                false,
                Verdict::Close(CloseReason::AuthDeadline),
            ),
            (AUTH_DEADLINE + MS, true, Verdict::Keep),
        ];
        for (elapsed, authenticated, expected) in cases {
            assert_eq!(
                auth_verdict(t0, authenticated, t0 + elapsed, AUTH_DEADLINE),
                expected,
                "elapsed {elapsed:?}, authenticated {authenticated}"
            );
        }
    }

    #[test]
    fn pong_verdict_closes_only_past_the_deadline_of_an_unanswered_ping() {
        let t0 = Instant::now();
        let cases = [
            // (unanswered ping sent at, elapsed, expected)
            (None, PONG_DEADLINE * 10, Verdict::Keep),
            (Some(t0), PONG_DEADLINE - MS, Verdict::Keep),
            (Some(t0), PONG_DEADLINE, Verdict::Keep),
            (
                Some(t0),
                PONG_DEADLINE + MS,
                Verdict::Close(CloseReason::PongDeadline),
            ),
        ];
        for (sent, elapsed, expected) in cases {
            assert_eq!(
                pong_verdict(sent, t0 + elapsed, PONG_DEADLINE),
                expected,
                "sent {sent:?}, elapsed {elapsed:?}"
            );
        }
    }

    /// The deadline runs from the oldest unanswered ping, and any inbound
    /// frame answers every ping so far.
    #[test]
    fn liveness_measures_from_the_oldest_unanswered_ping() {
        let t0 = Instant::now();
        let live = Liveness::new(t0);
        live.on_authenticated();

        live.on_ping_sent(t0);
        live.on_ping_sent(t0 + PING_INTERVAL);
        assert_eq!(
            live.verdict(t0 + PONG_DEADLINE + MS),
            Verdict::Close(CloseReason::PongDeadline),
            "a later ping must not push the deadline back"
        );

        live.on_inbound();
        assert_eq!(live.verdict(t0 + PONG_DEADLINE + MS), Verdict::Keep);
    }

    /// An idle unauthenticated connection is closed by the watchdog once the
    /// auth deadline has passed, on the paused clock.
    #[tokio::test(start_paused = true)]
    async fn the_watchdog_closes_a_connection_that_never_authenticates() {
        let started = Instant::now();
        let (closer, mut signal) = crate::outbound::close_channel();
        let live = std::sync::Arc::new(Liveness::new(started));
        tokio::spawn(watchdog(live, closer));

        assert_eq!(signal.closed().await, CloseReason::AuthDeadline);
        let waited = started.elapsed();
        assert!(
            waited > AUTH_DEADLINE && waited <= AUTH_DEADLINE + WATCHDOG_TICK,
            "closed after {waited:?}"
        );
    }
}
