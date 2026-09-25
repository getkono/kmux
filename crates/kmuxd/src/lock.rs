//! Poison-tolerant locking for a pane's emulator state (issue #206).
//!
//! A `std::sync::Mutex` is poisoned when a holder panics. With
//! `lock().unwrap()`, every later lock of that pane's `term_state` then panics
//! too, so one panic inside the emulator (a relay task, a snapshot, an encode)
//! kills the pane for the rest of the daemon's life: no more diffs, no
//! snapshots, no attach. The daemon runs for months, so it recovers instead,
//! the same way [`ResilientWriter`](crate::log_writer::ResilientWriter) does
//! for the log file: log at `error!`, take the inner value, carry on.
//!
//! The emulator may be mid-update when its holder panicked. A grid that is
//! briefly wrong is repaired by the next diff or by a client resync; a pane
//! that never produces output again is not repaired by anything.

use std::sync::{Mutex, MutexGuard};

use tracing::error;

use crate::term_state::TermState;

/// Lock `mutex`, recovering its value if a previous holder panicked.
///
/// The poison flag is cleared on recovery, so a pane logs one `error!` per
/// panic rather than one per later lock. `what` names the lock in the log.
pub(crate) fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            error!(
                lock = what,
                "lock poisoned by a panicking holder; recovering and continuing"
            );
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

/// Lock a pane's emulator state. The one entry point every `term_state` lock
/// goes through, so none of them can panic on poison.
pub(crate) fn lock_term_state(term_state: &Mutex<TermState>) -> MutexGuard<'_, TermState> {
    lock_or_recover(term_state, "term_state")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::diff_engine::DiffResult;
    use crate::fixtures::fixture_term_state;

    /// Poison `mutex` the way production would: a holder panics mid-update.
    fn poison<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
        let held = Arc::clone(mutex);
        let joined = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("a holder panicked mid-update");
        })
        .join();
        assert!(joined.is_err(), "the holder thread must have panicked");
        assert!(mutex.is_poisoned(), "precondition: the lock is poisoned");
    }

    #[test]
    fn lock_or_recover_returns_the_value_a_panicking_holder_left() {
        let mutex = Arc::new(Mutex::new(41));
        poison(&mutex);
        *lock_or_recover(&mutex, "counter") += 1;
        assert_eq!(*lock_or_recover(&mutex, "counter"), 42);
        assert!(!mutex.is_poisoned(), "recovery clears the poison flag");
    }

    /// The pane survives: after a poisoning panic its emulator still takes
    /// bytes and still produces the diff for them.
    #[test]
    fn a_poisoned_term_state_keeps_producing_diffs() {
        let ts = fixture_term_state(4, 20);
        poison(&ts);

        let mut guard = lock_term_state(&ts);
        guard.feed(b"still alive");
        let diff = guard.compute_diff();
        drop(guard);

        match diff {
            DiffResult::CellDiff { diff, .. } => assert!(!diff.ops.is_empty()),
            other => panic!("expected a cell diff, got {other:?}"),
        }
        assert!(!ts.is_poisoned());
    }
}
