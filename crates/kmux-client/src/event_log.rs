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

    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.entries.len()
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

    #[test]
    fn diag_event_display() {
        let ev = DiagEvent::StaleDiscard {
            session: "main".into(),
        };
        assert_eq!(ev.to_string(), "Stale discard on 'main'");

        let ev = DiagEvent::SeqnoGap {
            session: "main".into(),
            expected: 42,
            got: 44,
        };
        assert_eq!(ev.to_string(), "Seqno gap: 42\u{2192}44 on 'main'");

        let ev = DiagEvent::Lagged {
            session: "main".into(),
            missed: 5,
        };
        assert_eq!(ev.to_string(), "Lagged on 'main': missed 5");

        let ev = DiagEvent::Resync {
            session: "main".into(),
            reason: "seqno gap".into(),
        };
        assert_eq!(ev.to_string(), "Resync 'main': seqno gap");
    }

    #[test]
    fn counters_increment() {
        let mut c = DiagCounters::default();
        c.increment(&DiagEvent::StaleDiscard {
            session: "a".into(),
        });
        c.increment(&DiagEvent::StaleDiscard {
            session: "b".into(),
        });
        c.increment(&DiagEvent::SeqnoGap {
            session: "a".into(),
            expected: 1,
            got: 3,
        });
        c.increment(&DiagEvent::Lagged {
            session: "a".into(),
            missed: 2,
        });
        c.increment(&DiagEvent::Resync {
            session: "a".into(),
            reason: "lag".into(),
        });
        assert_eq!(c.stale_discards, 2);
        assert_eq!(c.seqno_gaps, 1);
        assert_eq!(c.lag_events, 1);
        assert_eq!(c.resyncs, 1);
    }

    #[test]
    fn event_log_push_and_recent() {
        let mut log = EventLog::new();
        log.push(DiagEvent::Resync {
            session: "a".into(),
            reason: "test".into(),
        });
        log.push(DiagEvent::Resync {
            session: "b".into(),
            reason: "test".into(),
        });
        log.push(DiagEvent::Resync {
            session: "c".into(),
            reason: "test".into(),
        });

        let recent: Vec<_> = log.recent(2).collect();
        assert_eq!(recent.len(), 2);
        assert!(matches!(&recent[0].1, DiagEvent::Resync { .. }));
        assert!(matches!(&recent[1].1, DiagEvent::Resync { .. }));
    }

    #[test]
    fn event_log_eviction() {
        let mut log = EventLog {
            entries: VecDeque::with_capacity(3),
            capacity: 3,
        };
        log.push(DiagEvent::Resync {
            session: "a".into(),
            reason: "1".into(),
        });
        log.push(DiagEvent::Resync {
            session: "b".into(),
            reason: "2".into(),
        });
        log.push(DiagEvent::Resync {
            session: "c".into(),
            reason: "3".into(),
        });
        log.push(DiagEvent::Resync {
            session: "d".into(),
            reason: "4".into(),
        }); // evicts first
        assert_eq!(log.len(), 3);
        let first = log.recent(3).next().unwrap();
        assert!(matches!(&first.1, DiagEvent::Resync { .. }));
    }

    #[test]
    fn diag_snapshot_from_log() {
        let mut log = EventLog::new();
        log.push(DiagEvent::StaleDiscard {
            session: "main".into(),
        });
        log.push(DiagEvent::Resync {
            session: "main".into(),
            reason: "test".into(),
        });

        let snap = DiagSnapshot::from_log(&log, 5);
        assert_eq!(snap.events.len(), 2);
        assert_eq!(snap.events[1].1, "Resync 'main': test");
    }
}
