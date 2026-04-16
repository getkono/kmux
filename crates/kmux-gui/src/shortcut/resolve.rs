use kmux_client::input::signal_from_key;

use super::{CommandEntry, SHORTCUTS, ShortcutAction, is_leader_key};

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
            "f" => Some(ShortcutAction::ToggleSnapshotMode),
            ":" => Some(ShortcutAction::OpenCommandPalette),
            "[" => Some(ShortcutAction::ScrollPageUp),
            "]" => Some(ShortcutAction::ScrollPageDown),
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
        iced::keyboard::Key::Character(c) => signal_from_key(c.as_str()),
        _ => None,
    }
}

/// All commands available in the command palette. Derived from [`SHORTCUTS`].
fn all_commands() -> Vec<CommandEntry> {
    SHORTCUTS
        .iter()
        .map(|s| CommandEntry {
            label: s.label.to_string(),
            shortcut_hint: s.palette_key.to_string(),
            action: s.action.clone(),
        })
        .collect()
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
}
