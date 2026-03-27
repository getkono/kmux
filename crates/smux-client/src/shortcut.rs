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
    OpenCommandPalette,
    SendLiteralLeader,
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

/// Check whether the given key+modifiers is the leader key (Ctrl+B).
pub fn is_leader_key(key: &iced::keyboard::Key, modifiers: iced::keyboard::Modifiers) -> bool {
    modifiers.control()
        && matches!(
            key,
            iced::keyboard::Key::Character(c) if c.as_str() == "b"
        )
}

/// In AwaitingAction state, resolve a key press to a `ShortcutAction`.
pub fn resolve_key(
    key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> Option<ShortcutAction> {
    use iced::keyboard::Key;
    use iced::keyboard::key::Named;

    // Ctrl+B again → send literal
    if is_leader_key(key, modifiers) {
        return Some(ShortcutAction::SendLiteralLeader);
    }

    match key {
        Key::Character(c) => match c.as_str() {
            "c" => Some(ShortcutAction::CreateSession),
            "x" => Some(ShortcutAction::CloseSession),
            "n" => Some(ShortcutAction::NextSession),
            "p" => Some(ShortcutAction::PrevSession),
            "," => Some(ShortcutAction::RenameSession),
            "d" => Some(ShortcutAction::Disconnect),
            "k" => Some(ShortcutAction::ShowSignalMenu),
            "l" => Some(ShortcutAction::ToggleInputLock),
            "?" => Some(ShortcutAction::ShowHelp),
            "h" => Some(ShortcutAction::ToggleHud),
            ":" => Some(ShortcutAction::OpenCommandPalette),
            "0" => Some(ShortcutAction::JumpToSession(9)), // 0 = 10th session
            "1" => Some(ShortcutAction::JumpToSession(0)),
            "2" => Some(ShortcutAction::JumpToSession(1)),
            "3" => Some(ShortcutAction::JumpToSession(2)),
            "4" => Some(ShortcutAction::JumpToSession(3)),
            "5" => Some(ShortcutAction::JumpToSession(4)),
            "6" => Some(ShortcutAction::JumpToSession(5)),
            "7" => Some(ShortcutAction::JumpToSession(6)),
            "8" => Some(ShortcutAction::JumpToSession(7)),
            "9" => Some(ShortcutAction::JumpToSession(8)),
            _ => None,
        },
        Key::Named(Named::ArrowRight) => Some(ShortcutAction::NextSession),
        Key::Named(Named::ArrowLeft) => Some(ShortcutAction::PrevSession),
        _ => None,
    }
}

/// Map a signal menu key to a Unix signal number.
pub fn resolve_signal_key(key: &iced::keyboard::Key) -> Option<i32> {
    match key {
        iced::keyboard::Key::Character(c) => match c.as_str() {
            "k" => Some(9),  // SIGKILL
            "t" => Some(15), // SIGTERM
            "s" => Some(19), // SIGSTOP
            "c" => Some(18), // SIGCONT
            _ => None,
        },
        _ => None,
    }
}

/// Entry in the help overlay.
pub fn shortcut_help_entries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("c", "Create new session"),
        ("x", "Close current session"),
        ("n / \u{2192}", "Next session"),
        ("p / \u{2190}", "Previous session"),
        ("0-9", "Jump to session by index"),
        (",", "Rename current session"),
        ("d", "Disconnect from server"),
        ("k", "Signal menu"),
        ("l", "Toggle input lock"),
        ("?", "Show this help"),
        ("h", "Toggle HUD metrics"),
        (":", "Command palette"),
        ("Ctrl+B", "Send literal Ctrl+B"),
    ]
}

/// A command palette entry.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    pub label: String,
    pub shortcut_hint: String,
    pub action: ShortcutAction,
}

/// All commands available in the command palette.
fn all_commands() -> Vec<CommandEntry> {
    vec![
        CommandEntry {
            label: "Create Session".into(),
            shortcut_hint: "c".into(),
            action: ShortcutAction::CreateSession,
        },
        CommandEntry {
            label: "Close Session".into(),
            shortcut_hint: "x".into(),
            action: ShortcutAction::CloseSession,
        },
        CommandEntry {
            label: "Next Session".into(),
            shortcut_hint: "n".into(),
            action: ShortcutAction::NextSession,
        },
        CommandEntry {
            label: "Previous Session".into(),
            shortcut_hint: "p".into(),
            action: ShortcutAction::PrevSession,
        },
        CommandEntry {
            label: "Rename Session".into(),
            shortcut_hint: ",".into(),
            action: ShortcutAction::RenameSession,
        },
        CommandEntry {
            label: "Disconnect".into(),
            shortcut_hint: "d".into(),
            action: ShortcutAction::Disconnect,
        },
        CommandEntry {
            label: "Signal Menu".into(),
            shortcut_hint: "k".into(),
            action: ShortcutAction::ShowSignalMenu,
        },
        CommandEntry {
            label: "Toggle Input Lock".into(),
            shortcut_hint: "l".into(),
            action: ShortcutAction::ToggleInputLock,
        },
        CommandEntry {
            label: "Show Help".into(),
            shortcut_hint: "?".into(),
            action: ShortcutAction::ShowHelp,
        },
        CommandEntry {
            label: "Toggle HUD".into(),
            shortcut_hint: "h".into(),
            action: ShortcutAction::ToggleHud,
        },
    ]
}

/// Filter and score commands by a fuzzy query.
///
/// Each query character must appear in order somewhere in the label (case-insensitive).
/// Scoring: consecutive matches score higher, earlier matches score higher.
pub fn filter_commands(query: &str) -> Vec<CommandEntry> {
    if query.is_empty() {
        return all_commands();
    }

    let query_lower = query.to_lowercase();
    let mut scored: Vec<(i32, CommandEntry)> = all_commands()
        .into_iter()
        .filter_map(|entry| {
            let label_lower = entry.label.to_lowercase();
            fuzzy_score(&query_lower, &label_lower).map(|score| (score, entry))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().map(|(_, e)| e).collect()
}

/// Simple fuzzy matching: returns a score if all query chars appear in order.
fn fuzzy_score(query: &str, target: &str) -> Option<i32> {
    let mut score = 0i32;
    let mut target_iter = target.char_indices().peekable();
    let mut prev_matched_idx: Option<usize> = None;

    for qch in query.chars() {
        let mut found = false;
        for (idx, tch) in target_iter.by_ref() {
            if tch == qch {
                // Bonus for consecutive matches
                if let Some(prev) = prev_matched_idx
                    && idx == prev + 1
                {
                    score += 10;
                }
                // Bonus for earlier matches
                score += 5i32.saturating_sub(idx as i32);
                prev_matched_idx = Some(idx);
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }

    Some(score)
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

    #[test]
    fn test_resolve_key_create() {
        let key = iced::keyboard::Key::Character("c".into());
        assert_eq!(
            resolve_key(&key, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::CreateSession)
        );
    }

    #[test]
    fn test_resolve_key_close() {
        let key = iced::keyboard::Key::Character("x".into());
        assert_eq!(
            resolve_key(&key, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::CloseSession)
        );
    }

    #[test]
    fn test_resolve_key_next_prev() {
        let n = iced::keyboard::Key::Character("n".into());
        assert_eq!(
            resolve_key(&n, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::NextSession)
        );

        let right = iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowRight);
        assert_eq!(
            resolve_key(&right, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::NextSession)
        );

        let p = iced::keyboard::Key::Character("p".into());
        assert_eq!(
            resolve_key(&p, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::PrevSession)
        );

        let left = iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowLeft);
        assert_eq!(
            resolve_key(&left, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::PrevSession)
        );
    }

    #[test]
    fn test_resolve_key_jump() {
        let key = iced::keyboard::Key::Character("1".into());
        assert_eq!(
            resolve_key(&key, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::JumpToSession(0))
        );
        let key = iced::keyboard::Key::Character("0".into());
        assert_eq!(
            resolve_key(&key, iced::keyboard::Modifiers::empty()),
            Some(ShortcutAction::JumpToSession(9))
        );
    }

    #[test]
    fn test_resolve_key_leader_again() {
        let key = iced::keyboard::Key::Character("b".into());
        assert_eq!(
            resolve_key(&key, iced::keyboard::Modifiers::CTRL),
            Some(ShortcutAction::SendLiteralLeader)
        );
    }

    #[test]
    fn test_resolve_key_unknown() {
        let key = iced::keyboard::Key::Character("z".into());
        assert_eq!(resolve_key(&key, iced::keyboard::Modifiers::empty()), None);
    }

    #[test]
    fn test_resolve_signal_key() {
        assert_eq!(
            resolve_signal_key(&iced::keyboard::Key::Character("k".into())),
            Some(9)
        );
        assert_eq!(
            resolve_signal_key(&iced::keyboard::Key::Character("t".into())),
            Some(15)
        );
        assert_eq!(
            resolve_signal_key(&iced::keyboard::Key::Character("z".into())),
            None
        );
    }

    #[test]
    fn test_filter_commands_empty_query() {
        let results = filter_commands("");
        assert!(!results.is_empty());
    }

    #[test]
    fn test_filter_commands_exact() {
        let results = filter_commands("rename");
        assert!(!results.is_empty());
        assert_eq!(results[0].action, ShortcutAction::RenameSession);
    }

    #[test]
    fn test_filter_commands_fuzzy() {
        let results = filter_commands("ren");
        assert!(!results.is_empty());
        assert_eq!(results[0].action, ShortcutAction::RenameSession);
    }

    #[test]
    fn test_filter_commands_no_match() {
        let results = filter_commands("zzzzz");
        assert!(results.is_empty());
    }

    #[test]
    fn test_shortcut_help_entries() {
        let entries = shortcut_help_entries();
        assert!(entries.len() >= 10);
        assert_eq!(entries[0].0, "c");
    }
}
