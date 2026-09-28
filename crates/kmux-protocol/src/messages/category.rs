use std::fmt;

use serde::{Deserialize, Serialize};

/// Logical category of a protocol message, used to bucket network traffic
/// by purpose rather than just by transport. Six categories covering the
/// full `ClientMessage`/`ServerMessage` surface:
///
/// - `Shell`     — PTY data flow (keystrokes in, screen updates out)
/// - `Scrollback`— history hydration (`FetchHistory` / `HistoryLines` / `ScrollbackAppend`)
/// - `Liveness`  — Ping / Pong keep-alive in both directions
/// - `Control`   — session/pane lifecycle, input locks, lifecycle events, errors
/// - `Sync`      — resync signals (Lagged / `SyncReset`)
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

    pub fn all() -> &'static [Self] {
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

    /// `all()` lists every category once, in display order, each shown by its
    /// name: one invariant over the whole enum (R9).
    #[test]
    fn all_lists_each_category_once_in_display_order() {
        let all = MessageCategory::all();
        let keys: Vec<u8> = all.iter().map(|c| c.as_sort_key()).collect();
        assert_eq!(keys, [0, 1, 2, 3, 4, 5]);
        let names: Vec<String> = all.iter().map(ToString::to_string).collect();
        let expected = [
            "Shell",
            "Scrollback",
            "Liveness",
            "Control",
            "Sync",
            "Bootstrap",
        ];
        assert_eq!(names, expected);
    }
}
