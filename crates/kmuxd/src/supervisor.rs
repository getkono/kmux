//! Restart long-lived daemon tasks that panic (issue #206).
//!
//! A bare `tokio::spawn` that panics just ends: the periodic checkpoint task
//! stopping meant sessions silently stopped being persisted for the rest of
//! the daemon's life, which on a server that never restarts is forever.
//! [`supervise`] runs such a task, and when it panics logs the payload at
//! `error!`, waits a backoff that grows with repeated failures
//! ([`restart_delay`]) and starts it again.

use std::any::Any;
use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{error, warn};

/// Delay before the first restart after a panic.
const RESTART_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Longest delay between restarts, however often the task keeps panicking.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// A run at least this long counts as healthy: a panic after it starts the
/// backoff over instead of continuing to grow it.
const STABLE_RUN: Duration = Duration::from_secs(60);

/// How long to wait before restart number `consecutive_failures` (1 for the
/// first panic in a row): `base`, doubling per further failure, capped at
/// `max`.
pub(crate) fn restart_delay(consecutive_failures: u32, base: Duration, max: Duration) -> Duration {
    let doublings = consecutive_failures.saturating_sub(1).min(31);
    base.saturating_mul(1 << doublings).min(max)
}

/// The message a panic carried, as best it can be recovered.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s
    } else {
        "non-string panic payload"
    }
}

/// Run the task `make` builds, restarting it after every panic, until a run
/// returns (or is cancelled). Returns how many restarts that took.
///
/// Each run is its own spawned task, so a panic is caught by the runtime and
/// seen here as a `JoinError`; the task must hold nothing a panic could leave
/// half-updated for the next run (the tasks supervised today re-read all their
/// state from `ServerApp` every iteration).
pub(crate) async fn supervise<F, Fut>(name: &'static str, mut make: F) -> u32
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut restarts = 0u32;
    let mut consecutive_failures = 0u32;
    loop {
        let started = Instant::now();
        let err = match tokio::spawn(make()).await {
            Ok(()) => return restarts,
            Err(err) if err.is_panic() => err,
            Err(_) => {
                warn!(task = name, "supervised task was cancelled");
                return restarts;
            }
        };
        if started.elapsed() >= STABLE_RUN {
            consecutive_failures = 0;
        }
        consecutive_failures += 1;
        restarts += 1;
        let delay = restart_delay(
            consecutive_failures,
            RESTART_BACKOFF_BASE,
            RESTART_BACKOFF_MAX,
        );
        let payload = err.into_panic();
        error!(
            task = name,
            panic = panic_message(payload.as_ref()),
            restarts,
            ?delay,
            "supervised task panicked; restarting it"
        );
        tokio::time::sleep(delay).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn restart_delay_doubles_per_consecutive_failure_up_to_the_cap() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(60);
        let cases = [(1, 1), (2, 2), (3, 4), (6, 32), (7, 60), (1000, 60)];
        for (failures, secs) in cases {
            assert_eq!(
                restart_delay(failures, base, max),
                Duration::from_secs(secs),
                "after {failures} failures"
            );
        }
    }

    #[test]
    fn panic_message_reads_both_payload_kinds() {
        assert_eq!(panic_message(&"static"), "static");
        assert_eq!(panic_message(&String::from("formatted")), "formatted");
        assert_eq!(panic_message(&42u8), "non-string panic payload");
    }

    /// A task that panics once and then completes is run twice, with one
    /// restart after the first backoff (on the paused clock).
    #[tokio::test(start_paused = true)]
    async fn supervise_reruns_a_task_that_panicked_and_counts_the_restart() {
        let runs = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&runs);
        let started = Instant::now();

        let restarts = supervise("test", move || {
            let runs = Arc::clone(&counted);
            async move {
                let first_run = runs.fetch_add(1, Ordering::SeqCst) == 0;
                assert!(!first_run, "first run fails");
            }
        })
        .await;

        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(restarts, 1);
        assert!(
            started.elapsed() >= RESTART_BACKOFF_BASE,
            "backed off first"
        );
    }
}
