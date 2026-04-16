mod help;
mod resolve;
pub use help::shortcut_help_entries;
pub use resolve::{filter_commands, resolve_key, resolve_signal_key};

use std::time::{Duration, Instant};

/// Timeout before the leader key prefix is automatically cancelled.
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(1);

/// Actions that can be dispatched from the leader key system or command palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutAction {
    CreateSession,
    CloseSession,
    NextSession,
    PrevSession,
    JumpToSession(usize),
    RenameSession,
    Disconnect,
    ShowSignalMenu,
    ToggleInputLock,
    ShowHelp,
    ToggleHud,
    ToggleSnapshotMode,
    OpenCommandPalette,
    SendLiteralLeader,
    ScrollPageUp,
    ScrollPageDown,
}

/// Leader key state machine.
#[derive(Debug, Default)]
pub enum LeaderState {
    #[default]
    Idle,
    AwaitingAction {
        entered_at: Instant,
    },
    RenameEditing {
        buffer: String,
        session: String,
    },
    ConfirmClose {
        session: String,
    },
    SignalMenu {
        session: String,
    },
    HelpVisible,
    CommandPalette {
        query: String,
        selected: usize,
    },
}

impl LeaderState {
    pub fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub fn is_awaiting_action(&self) -> bool {
        matches!(self, Self::AwaitingAction { .. })
    }

    pub fn is_leader_active(&self) -> bool {
        !self.is_idle()
    }
}

/// A command palette entry.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub label: String,
    pub shortcut_hint: String,
    pub action: ShortcutAction,
}

/// Single source of truth for all leader-key shortcuts that appear in both
/// the command palette and the help overlay.
pub(super) struct ShortcutDef {
    /// Short key shown in the command palette (e.g. `"n"`).
    pub palette_key: &'static str,
    /// Key label shown in the help overlay (may include aliases, e.g. `"n / →"`).
    pub help_key: &'static str,
    /// Title-case label for the command palette.
    pub label: &'static str,
    /// Lower-case description for the help overlay.
    pub help_desc: &'static str,
    pub action: ShortcutAction,
}

pub(super) static SHORTCUTS: &[ShortcutDef] = &[
    ShortcutDef {
        palette_key: "c",
        help_key: "c",
        label: "Create Session",
        help_desc: "Create new session",
        action: ShortcutAction::CreateSession,
    },
    ShortcutDef {
        palette_key: "x",
        help_key: "x",
        label: "Close Session",
        help_desc: "Close current session",
        action: ShortcutAction::CloseSession,
    },
    ShortcutDef {
        palette_key: "n",
        help_key: "n / \u{2192}",
        label: "Next Session",
        help_desc: "Next session",
        action: ShortcutAction::NextSession,
    },
    ShortcutDef {
        palette_key: "p",
        help_key: "p / \u{2190}",
        label: "Previous Session",
        help_desc: "Previous session",
        action: ShortcutAction::PrevSession,
    },
    ShortcutDef {
        palette_key: ",",
        help_key: ",",
        label: "Rename Session",
        help_desc: "Rename current session",
        action: ShortcutAction::RenameSession,
    },
    ShortcutDef {
        palette_key: "d",
        help_key: "d",
        label: "Disconnect",
        help_desc: "Disconnect from server",
        action: ShortcutAction::Disconnect,
    },
    ShortcutDef {
        palette_key: "k",
        help_key: "k",
        label: "Signal Menu",
        help_desc: "Signal menu",
        action: ShortcutAction::ShowSignalMenu,
    },
    ShortcutDef {
        palette_key: "l",
        help_key: "l",
        label: "Toggle Input Lock",
        help_desc: "Toggle input lock",
        action: ShortcutAction::ToggleInputLock,
    },
    ShortcutDef {
        palette_key: "?",
        help_key: "?",
        label: "Show Help",
        help_desc: "Show this help",
        action: ShortcutAction::ShowHelp,
    },
    ShortcutDef {
        palette_key: "h",
        help_key: "h",
        label: "Toggle HUD",
        help_desc: "Toggle HUD metrics",
        action: ShortcutAction::ToggleHud,
    },
    ShortcutDef {
        palette_key: "f",
        help_key: "f",
        label: "Toggle Snapshot Mode",
        help_desc: "Toggle full-snapshot mode",
        action: ShortcutAction::ToggleSnapshotMode,
    },
];

/// Help-only entries that have no command-palette action.
pub(super) const HELP_ONLY: &[(&str, &str)] = &[
    ("0-9", "Jump to session by index"),
    (":", "Command palette"),
    ("[", "Scroll page up"),
    ("]", "Scroll page down"),
    ("Shift+PgUp/Dn", "Scroll page (direct)"),
    ("Cmd+C / Ctrl+Shift+C", "Copy selection"),
    ("Cmd+V / Ctrl+Shift+V", "Paste from clipboard"),
    ("Ctrl+B", "Send literal Ctrl+B"),
];

/// Check whether the given key+modifiers is the leader key (Ctrl+B).
pub fn is_leader_key(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> bool {
    modifiers.control()
        && matches!(
            key,
            iced::keyboard::Key::Character(c) if c.as_str() == "b"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_leader_key() {
        let key = iced::keyboard::Key::Character("b".into());
        let mods = iced::keyboard::Modifiers::CTRL;
        assert!(is_leader_key(&key, mods));
    }

    #[test]
    fn test_is_leader_key_negative() {
        // No ctrl modifier
        let key = iced::keyboard::Key::Character("b".into());
        assert!(!is_leader_key(&key, iced::keyboard::Modifiers::empty()));

        // Wrong key
        let key = iced::keyboard::Key::Character("a".into());
        let mods = iced::keyboard::Modifiers::CTRL;
        assert!(!is_leader_key(&key, mods));
    }
}
