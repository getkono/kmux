//! Reconnect backoff (issue #208): how long to wait before the next attempt to
//! re-establish a dropped link.
//!
//! One policy, shared by everything that reconnects on its own — the GUI's
//! link to its local daemon (`kmux-app`) and a daemon's federation link to a
//! peer (`kmuxd`) — so the two back off the same way.
//!
//! The delay doubles per attempt from [`BACKOFF_MIN`] up to [`BACKOFF_MAX`].
//! Jitter spreads reconnects that lost their link at the same moment (every
//! GUI of a restarted daemon, every hub of a rebooted peer): it shortens a
//! delay by up to [`BACKOFF_JITTER_CAP_PERMILLE`]‰, never lengthens it. Since
//! the jitter fraction depends only on the seed, one seed's delays never
//! shrink from one attempt to the next.

use std::time::Duration;

/// The delay before the first retry (attempt 0), before jitter.
pub const BACKOFF_MIN: Duration = Duration::from_millis(250);

/// The longest delay between two attempts: the doubling stops here.
pub const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// The most jitter may shorten a delay by, in thousandths of it.
pub const BACKOFF_JITTER_CAP_PERMILLE: u32 = 200;

/// The delay before retry `attempt` (0-based), jittered by `jitter_seed`.
///
/// `BACKOFF_MIN · 2^attempt`, capped at [`BACKOFF_MAX`], less a fraction of it
/// below [`BACKOFF_JITTER_CAP_PERMILLE`] drawn from `jitter_seed`. Pure: the
/// caller picks the seed once per link (see [`jitter_seed`]) and passes it
/// with every attempt.
pub fn next_delay(attempt: u32, jitter_seed: u64) -> Duration {
    let base = BACKOFF_MIN
        .checked_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .map_or(BACKOFF_MAX, |d| d.min(BACKOFF_MAX));
    let permille = jitter_seed % (u64::from(BACKOFF_JITTER_CAP_PERMILLE) + 1);
    let permille = u32::try_from(permille).unwrap_or(BACKOFF_JITTER_CAP_PERMILLE);
    base * (1000 - permille) / 1000
}

/// A fresh jitter seed for one link.
pub fn jitter_seed() -> u64 {
    rand::random()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no jitter the delay doubles from the minimum and holds at the
    /// maximum, however many attempts follow.
    #[test]
    fn next_delay_without_jitter_doubles_up_to_the_maximum() {
        let table = [
            (0, 250),
            (1, 500),
            (2, 1_000),
            (3, 2_000),
            (4, 4_000),
            (5, 8_000),
            (6, 15_000),
            (7, 15_000),
            (40, 15_000),
            (u32::MAX, 15_000),
        ];
        for (attempt, millis) in table {
            assert_eq!(
                next_delay(attempt, 0),
                Duration::from_millis(millis),
                "attempt {attempt}"
            );
        }
    }

    /// For any one seed the delays never shrink from one attempt to the next,
    /// and every delay lies within the jitter cap below its unjittered value.
    #[test]
    fn next_delay_is_monotonic_per_seed_and_jitter_stays_under_the_cap() {
        for seed in [0, 1, 57, 200, 201, 999, u64::MAX] {
            let mut previous = Duration::ZERO;
            for attempt in 0..12 {
                let delay = next_delay(attempt, seed);
                let unjittered = next_delay(attempt, 0);
                assert!(delay >= previous, "seed {seed}, attempt {attempt}");
                assert!(delay <= unjittered, "seed {seed}, attempt {attempt}");
                assert!(
                    delay >= unjittered.mul_f64(0.8),
                    "seed {seed}, attempt {attempt}: {delay:?} under the cap"
                );
                previous = delay;
            }
        }
    }

    /// Each link draws its own seed, so links that dropped together spread.
    #[test]
    fn jitter_seed_differs_between_links() {
        assert_ne!(jitter_seed(), jitter_seed());
    }

    /// The most a seed can take off is the cap itself.
    #[test]
    fn next_delay_at_the_largest_jitter_takes_off_exactly_the_cap() {
        assert_eq!(
            next_delay(0, u64::from(BACKOFF_JITTER_CAP_PERMILLE)),
            Duration::from_millis(200)
        );
        assert_eq!(
            next_delay(0, u64::from(BACKOFF_JITTER_CAP_PERMILLE) + 1),
            BACKOFF_MIN,
            "the fraction wraps rather than exceeding the cap"
        );
    }
}
