//! Forward libghostty-vt's diagnostic logging into kmux tracing (issue #187).
//!
//! libghostty-vt logs every control sequence it cannot handle ("unimplemented
//! CSI/ESC/OSC action", and similar) through Zig's `std.log`. The kmux Zig
//! wrapper routes that output to a C callback; this module installs the callback
//! once and re-emits each line under the `kmux::vt` tracing target, so unknown
//! sequences land in the daemon log (or an isolated worker's output) for
//! `kmux daemon logs` to surface — the user-facing half of issue #187.
//!
//! A misbehaving program can emit the same unhandled sequence in a tight loop,
//! so identical messages are de-duplicated: the first is logged in full, then
//! repeats are suppressed and only periodically summarised, with the tracked-set
//! size bounded so a flood of *distinct* messages cannot grow memory unbounded.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use kmux_ghostty::{VtLogLevel, set_log_handler};

/// Cap on distinct messages tracked for de-dup, bounding memory under a churn of
/// many different unhandled sequences.
const MAX_DEDUP_ENTRIES: usize = 512;
/// Emit a "still happening" summary every this-many suppressed repeats.
const REPEAT_SUMMARY_EVERY: u32 = 256;

/// Install the VT-log forwarder. Idempotent — call once per process at startup
/// (the daemon and each isolated VT worker do, after their tracing subscriber is
/// initialized).
pub fn install_vt_log_forwarding() {
    set_log_handler(forward);
}

fn dedup() -> &'static Mutex<HashMap<u64, u32>> {
    static D: OnceLock<Mutex<HashMap<u64, u32>>> = OnceLock::new();
    D.get_or_init(|| Mutex::new(HashMap::new()))
}

/// What to do with a message after consulting the de-dup table.
#[derive(Debug, PartialEq, Eq)]
enum Emit {
    /// First time seen — log it in full.
    First,
    /// Seen `n` times — log a periodic "still happening" summary.
    Repeat(u32),
    /// A repeat between summaries — drop it.
    Suppress,
}

/// Decide how to emit the message identified by `key`, updating `counts`.
fn classify(counts: &mut HashMap<u64, u32>, key: u64) -> Emit {
    // Bound memory: if a flood of distinct messages fills the table, reset it
    // rather than grow without limit. Worst case re-logs a message once more.
    if !counts.contains_key(&key) && counts.len() >= MAX_DEDUP_ENTRIES {
        counts.clear();
    }
    match counts.get_mut(&key) {
        None => {
            counts.insert(key, 0);
            Emit::First
        }
        Some(c) => {
            *c += 1;
            if *c % REPEAT_SUMMARY_EVERY == 0 {
                Emit::Repeat(*c)
            } else {
                Emit::Suppress
            }
        }
    }
}

fn forward(level: VtLogLevel, scope: &str, msg: &str) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    scope.hash(&mut hasher);
    msg.hash(&mut hasher);
    let key = hasher.finish();

    let emit = {
        let mut counts = dedup()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        classify(&mut counts, key)
    };

    match emit {
        Emit::First => log_at(level, scope, msg, None),
        Emit::Repeat(n) => log_at(level, scope, msg, Some(n)),
        Emit::Suppress => {}
    }
}

fn log_at(level: VtLogLevel, scope: &str, msg: &str, repeated: Option<u32>) {
    let suffix = match repeated {
        Some(n) => format!(" (repeated {n}×)"),
        None => String::new(),
    };
    match level {
        VtLogLevel::Error => tracing::error!(target: "kmux::vt", vt_scope = scope, "{msg}{suffix}"),
        VtLogLevel::Warn => tracing::warn!(target: "kmux::vt", vt_scope = scope, "{msg}{suffix}"),
        VtLogLevel::Info => tracing::info!(target: "kmux::vt", vt_scope = scope, "{msg}{suffix}"),
        VtLogLevel::Debug => tracing::debug!(target: "kmux::vt", vt_scope = scope, "{msg}{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_then_suppress_then_periodic_summary() {
        let mut counts = HashMap::new();
        assert_eq!(classify(&mut counts, 1), Emit::First);
        for _ in 1..REPEAT_SUMMARY_EVERY {
            assert_eq!(classify(&mut counts, 1), Emit::Suppress);
        }
        assert_eq!(classify(&mut counts, 1), Emit::Repeat(REPEAT_SUMMARY_EVERY));
        // A different message is logged on its own first sighting.
        assert_eq!(classify(&mut counts, 2), Emit::First);
    }

    #[test]
    fn tracked_set_is_bounded() {
        let mut counts = HashMap::new();
        for k in 0..MAX_DEDUP_ENTRIES as u64 {
            assert_eq!(classify(&mut counts, k), Emit::First);
        }
        assert_eq!(counts.len(), MAX_DEDUP_ENTRIES);
        // The next distinct key triggers a reset, then inserts itself.
        assert_eq!(classify(&mut counts, u64::MAX), Emit::First);
        assert_eq!(counts.len(), 1);
    }
}
