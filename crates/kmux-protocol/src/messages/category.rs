use std::fmt;

use serde::{Deserialize, Serialize};

/// Logical category of a protocol message, used to bucket network traffic
/// by purpose rather than just by transport. Six categories covering the
/// full `ClientMessage`/`ServerMessage` surface:
///
/// - `Shell`     — PTY data flow (keystrokes in, screen updates out)
/// - `Scrollback`— history hydration (FetchHistory / HistoryLines / ScrollbackAppend)
/// - `Liveness`  — Ping / Pong keep-alive in both directions
/// - `Control`   — session/pane lifecycle, input locks, lifecycle events, errors
/// - `Sync`      — resync signals (Lagged / SyncReset)
/// - `Bootstrap` — authentication and transport-switch handshake
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum MessageCategory {
    Shell,
    Scrollback,
    Liveness,
    Control,
    Sync,
    Bootstrap,
}

impl MessageCategory {
    /// Stable display order for the overlay: Shell first (most traffic),
    /// Scrollback, Liveness, Control, Sync, Bootstrap last (least frequent).
    pub fn as_sort_key(self) -> u8 {
        match self {
            Self::Shell => 0,
            Self::Scrollback => 1,
            Self::Liveness => 2,
            Self::Control => 3,
            Self::Sync => 4,
            Self::Bootstrap => 5,
        }
    }

    pub fn all() -> &'static [MessageCategory] {
        &[
            Self::Shell,
            Self::Scrollback,
            Self::Liveness,
            Self::Control,
            Self::Sync,
            Self::Bootstrap,
        ]
    }
}

impl fmt::Display for MessageCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Shell => "Shell",
            Self::Scrollback => "Scrollback",
            Self::Liveness => "Liveness",
            Self::Control => "Control",
            Self::Sync => "Sync",
            Self::Bootstrap => "Bootstrap",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_has_six_distinct_categories() {
        let all = MessageCategory::all();
        assert_eq!(all.len(), 6);
        let mut seen = std::collections::HashSet::new();
        for c in all {
            assert!(seen.insert(c), "duplicate category: {c}");
        }
    }

    #[test]
    fn sort_keys_are_unique() {
        let keys: Vec<u8> = MessageCategory::all()
            .iter()
            .map(|c| c.as_sort_key())
            .collect();
        let mut uniq = keys.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(keys.len(), uniq.len());
    }

    #[test]
    fn display_roundtrips() {
        for c in MessageCategory::all() {
            assert!(!c.to_string().is_empty());
        }
    }
}
