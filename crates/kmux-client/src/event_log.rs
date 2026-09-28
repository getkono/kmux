use std::collections::VecDeque;
use std::fmt;
use std::time::Instant;

const EVENT_LOG_CAPACITY: usize = 32;

/// Disruptive event types that can cause missed or delayed renders.
#[derive(Clone, Debug)]
pub enum DiagEvent {
    StaleDiscard {
        session: String,
    },
    SeqnoGap {
        session: String,
        expected: u64,
        got: u64,
    },
    Lagged {
        session: String,
        missed: u64,
    },
    Resync {
        session: String,
        reason: String,
    },
    /// A partial logical frame was painted: this tick applied a cell diff whose
    /// daemon `sent_at_ms` was within the coalescing window of the diff painted
    /// last tick, so the previous paint showed an incomplete frame (issue #72).
    Tear {
        session: String,
        prev_sent_at_ms: u64,
        next_sent_at_ms: u64,
    },
    /// The daemon's authoritative grid digest for a seqno did not match the
    /// client's reconstructed grid: the diff stream desynced. The client
    /// resyncs. In a correct pipeline this never fires; the conformance and
    /// e2e suites assert the count stays zero.
    DigestMismatch {
        session: String,
        seqno: u64,
    },
}

impl fmt::Display for DiagEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleDiscard { session } => {
                write!(f, "Stale discard on '{session}'")
            }
            Self::SeqnoGap {
                session,
                expected,
                got,
            } => {
                write!(f, "Seqno gap: {expected}\u{2192}{got} on '{session}'")
            }
            Self::Lagged { session, missed } => {
                write!(f, "Lagged on '{session}': missed {missed}")
            }
            Self::Resync { session, reason } => {
                write!(f, "Resync '{session}': {reason}")
            }
            Self::Tear {
                session,
                prev_sent_at_ms,
                next_sent_at_ms,
            } => {
                write!(
                    f,
                    "Tear on '{session}': {prev_sent_at_ms}\u{2192}{next_sent_at_ms}ms"
                )
            }
            Self::DigestMismatch { session, seqno } => {
                write!(f, "Grid digest mismatch on '{session}' at seqno {seqno}")
            }
        }
    }
}

/// Counters for disruptive events. `Copy` so it can live in `MetricsSnapshot`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiagCounters {
    pub stale_discards: u64,
    pub seqno_gaps: u64,
    pub lag_events: u64,
    pub resyncs: u64,
    /// Partial logical frames painted (issue #72 tearing detector).
    pub tears: u64,
    /// Grid-digest mismatches detected against the daemon's authoritative grid.
    /// Expected to stay zero; non-zero means the diff stream desynced.
    pub digest_mismatches: u64,
}

impl DiagCounters {
    pub fn increment(&mut self, event: &DiagEvent) {
        match event {
            DiagEvent::StaleDiscard { .. } => self.stale_discards += 1,
            DiagEvent::SeqnoGap { .. } => self.seqno_gaps += 1,
            DiagEvent::Lagged { .. } => self.lag_events += 1,
            DiagEvent::Resync { .. } => self.resyncs += 1,
            DiagEvent::Tear { .. } => self.tears += 1,
            DiagEvent::DigestMismatch { .. } => self.digest_mismatches += 1,
        }
    }
}

/// Rolling log of timestamped diagnostic events.
pub struct EventLog {
    entries: VecDeque<(Instant, DiagEvent)>,
    capacity: usize,
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(EVENT_LOG_CAPACITY),
            capacity: EVENT_LOG_CAPACITY,
        }
    }

    pub fn push(&mut self, event: DiagEvent) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((Instant::now(), event));
    }

    /// Returns the last `n` events, most recent last.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &(Instant, DiagEvent)> {
        let skip = self.entries.len().saturating_sub(n);
        self.entries.iter().skip(skip)
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

/// Snapshot of diagnostic state for the HUD. Pre-formatted event strings
/// avoid allocation in the hot `draw()` path.
#[derive(Clone)]
pub struct DiagSnapshot {
    pub events: Vec<(Instant, String)>,
}

impl DiagSnapshot {
    pub fn from_log(log: &EventLog, max_events: usize) -> Self {
        let events = log
            .recent(max_events)
            .map(|(ts, ev)| (*ts, ev.to_string()))
            .collect();
        Self { events }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resync(session: &str) -> DiagEvent {
        DiagEvent::Resync {
            session: session.into(),
            reason: "test".into(),
        }
    }

    fn log_of(capacity: usize, sessions: &[&str]) -> EventLog {
        let mut log = EventLog {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        };
        for s in sessions {
            log.push(resync(s));
        }
        log
    }

    /// The session of each of the last `n` events, oldest first.
    fn recent_sessions(log: &EventLog, n: usize) -> Vec<String> {
        log.recent(n)
            .map(|(_, ev)| match ev {
                DiagEvent::Resync { session, .. } => session.clone(),
                other => panic!("unexpected event {other:?}"),
            })
            .collect()
    }

    fn every_variant() -> [DiagEvent; 6] {
        let session = || "main".to_string();
        [
            DiagEvent::StaleDiscard { session: session() },
            DiagEvent::SeqnoGap {
                session: session(),
                expected: 42,
                got: 44,
            },
            DiagEvent::Lagged {
                session: session(),
                missed: 5,
            },
            DiagEvent::Resync {
                session: session(),
                reason: "seqno gap".into(),
            },
            DiagEvent::Tear {
                session: session(),
                prev_sent_at_ms: 1000,
                next_sent_at_ms: 1008,
            },
            DiagEvent::DigestMismatch {
                session: session(),
                seqno: 7,
            },
        ]
    }

    #[test]
    fn diag_event_display_every_variant_formats_its_fields() {
        let want = [
            "Stale discard on 'main'",
            "Seqno gap: 42\u{2192}44 on 'main'",
            "Lagged on 'main': missed 5",
            "Resync 'main': seqno gap",
            "Tear on 'main': 1000\u{2192}1008ms",
            "Grid digest mismatch on 'main' at seqno 7",
        ];
        for (ev, want) in every_variant().iter().zip(want) {
            assert_eq!(ev.to_string(), want, "{ev:?}");
        }
    }

    #[test]
    fn diag_counters_increment_each_variant_bumps_only_its_counter() {
        let mut c = DiagCounters::default();
        for (i, ev) in every_variant().iter().enumerate() {
            // Variant i is recorded i + 1 times so a crossed wire shows.
            for _ in 0..=i {
                c.increment(ev);
            }
        }
        assert_eq!(
            [
                c.stale_discards,
                c.seqno_gaps,
                c.lag_events,
                c.resyncs,
                c.tears,
                c.digest_mismatches
            ],
            [1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn event_log_recent_returns_the_last_n_oldest_first() {
        let log = log_of(EVENT_LOG_CAPACITY, &["a", "b", "c"]);
        assert_eq!(recent_sessions(&log, 2), ["b", "c"]);
        assert_eq!(recent_sessions(&log, 10), ["a", "b", "c"], "n past len");
    }

    #[test]
    fn event_log_push_at_capacity_evicts_the_oldest() {
        let log = log_of(3, &["a", "b", "c", "d"]);
        assert_eq!(recent_sessions(&log, usize::MAX), ["b", "c", "d"]);
    }

    #[test]
    fn diag_snapshot_from_log_formats_the_most_recent_events() {
        let mut log = EventLog::new();
        log.push(DiagEvent::StaleDiscard {
            session: "main".into(),
        });
        log.push(resync("main"));
        log.push(resync("other"));

        let snap = DiagSnapshot::from_log(&log, 2);
        let texts: Vec<&str> = snap.events.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(texts, ["Resync 'main': test", "Resync 'other': test"]);
    }
}
