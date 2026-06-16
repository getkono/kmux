use kmux_client::input::signal_from_key;
use kmux_client::key::{Key, Modifiers, NamedKey};

use super::{Action, Mode, is_mode_key};

/// True for the Ctrl+C cancel chord used by every text-input mode as a
/// defense-in-depth exit hatch (alongside Esc).
fn is_ctrl_c(key: &Key, mods: Modifiers) -> bool {
    mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "c")
}

pub fn resolve_normal(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Select), Action::None);
    }

    // Ctrl+Alt+R: force a reconnect even without dropping first. Useful when
    // the link is degraded but has not yet tripped the liveness timeout.
    if mods.contains(Modifiers::CTRL)
        && mods.contains(Modifiers::ALT)
        && matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("r"))
    {
        return (None, Action::Reconnect);
    }

    // Shift+PageUp/Down for scrollback
    if mods.contains(Modifiers::SHIFT) {
        if matches!(key, Key::Named(NamedKey::PageUp)) {
            return (None, Action::ScrollPageUp);
        }
        if matches!(key, Key::Named(NamedKey::PageDown)) {
            return (None, Action::ScrollPageDown);
        }
    }

    // Ctrl+Shift+C for copy
    if mods.contains(Modifiers::CTRL) && mods.contains(Modifiers::SHIFT) {
        if matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("c")) {
            return (None, Action::CopySelection);
        }
        if matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("v")) {
            return (None, Action::Paste);
        }
    }

    // Everything else forwards to PTY
    (None, Action::ForwardKey)
}

pub fn resolve_locked(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Normal), Action::None);
    }
    // Everything passes through in locked mode
    (None, Action::ForwardKey)
}

pub fn resolve_mode_select(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    // Command palette: bare `/` (or Ctrl+/ which kitty/Ghostty deliver as the
    // same `Char('/')`) or the legacy `\x1f` byte some terminals emit for
    // Ctrl+/. Either form lands here regardless of the CTRL modifier.
    let is_command_trigger = matches!(key, Key::Character(c) if c == "/")
        || matches!(key, Key::Character(c) if c == "\u{1f}");
    if is_command_trigger {
        return (
            Some(Mode::Command(super::CommandState::default())),
            Action::None,
        );
    }
    match key {
        Key::Character(c) => match c.as_str() {
            "s" => (Some(Mode::Session), Action::None),
            "o" => (Some(Mode::Scroll), Action::None),
            "k" => (Some(Mode::Signal), Action::None),
            "l" => (Some(Mode::Locked), Action::None),
            "h" => (Some(Mode::Normal), Action::ToggleHud),
            "m" => (Some(Mode::Normal), Action::ToggleMetrics),
            "r" => (Some(Mode::Normal), Action::ForceRedraw),
            "?" => (Some(Mode::Help), Action::None),
            "q" => (Some(Mode::Normal), Action::Quit),
            _ => (Some(Mode::Normal), Action::None),
        },
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (Some(Mode::Normal), Action::None),
    }
}

pub fn resolve_session(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) => match c.as_str() {
            "c" => (Some(Mode::Normal), Action::CreateSession),
            "p" => (Some(Mode::Normal), Action::CreatePane),
            "X" => (None, Action::CloseSession),
            "x" => (Some(Mode::Normal), Action::ClosePane),
            "n" => (None, Action::NextSession),
            "j" => (None, Action::NextPane),
            "k" => (None, Action::PrevPane),
            "r" => (None, Action::RenameSession),
            "d" => (Some(Mode::Normal), Action::Disconnect),
            "l" => (None, Action::ToggleInputLock),
            "f" => (None, Action::ToggleSnapshotMode),
            "P" => (None, Action::TogglePause),
            "0" => (Some(Mode::Normal), Action::JumpToSession(9)),
            "1" => (Some(Mode::Normal), Action::JumpToSession(0)),
            "2" => (Some(Mode::Normal), Action::JumpToSession(1)),
            "3" => (Some(Mode::Normal), Action::JumpToSession(2)),
            "4" => (Some(Mode::Normal), Action::JumpToSession(3)),
            "5" => (Some(Mode::Normal), Action::JumpToSession(4)),
            "6" => (Some(Mode::Normal), Action::JumpToSession(5)),
            "7" => (Some(Mode::Normal), Action::JumpToSession(6)),
            "8" => (Some(Mode::Normal), Action::JumpToSession(7)),
            "9" => (Some(Mode::Normal), Action::JumpToSession(8)),
            _ => (None, Action::None),
        },
        Key::Named(NamedKey::Tab) => (None, Action::NextPane),
        Key::Named(NamedKey::ArrowRight) => (None, Action::NextSession),
        Key::Named(NamedKey::ArrowLeft) => (None, Action::PrevSession),
        Key::Named(NamedKey::ArrowDown) => (None, Action::NextPane),
        Key::Named(NamedKey::ArrowUp) => (None, Action::PrevPane),
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (None, Action::None),
    }
}

pub fn resolve_scroll(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Named(NamedKey::ArrowUp) => (None, Action::ScrollUp(1)),
        Key::Named(NamedKey::ArrowDown) => (None, Action::ScrollDown(1)),
        Key::Named(NamedKey::PageUp) => (None, Action::ScrollPageUp),
        Key::Named(NamedKey::PageDown) => (None, Action::ScrollPageDown),
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::ExitToNormal),
        Key::Character(c) if c == "q" => (Some(Mode::Normal), Action::ExitToNormal),
        _ => (None, Action::None),
    }
}

pub fn resolve_signal(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) => {
            let action = match signal_from_key(c.as_str()) {
                Some(sig) => Action::SendSignal(sig),
                None => Action::None,
            };
            (Some(Mode::Normal), action)
        }
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (None, Action::None),
    }
}

pub fn resolve_confirm_close(key: &Key) -> (Option<Mode>, Action) {
    match key {
        // Return None for mode: the action handler owns the transition so it
        // can extract word_id via mem::replace before setting Mode::Normal.
        Key::Character(c) if c == "y" => (None, Action::ConfirmCloseYes),
        _ => (Some(Mode::Normal), Action::None),
    }
}

/// Keys accepted while disconnected. Everything else is dropped (so pane
/// input is effectively frozen) and the overlay stays up.
pub fn resolve_disconnected(key: &Key) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) if c == "y" || c == "Y" => (None, Action::Reconnect),
        Key::Named(NamedKey::Enter) => (None, Action::Reconnect),
        Key::Character(c) if c == "q" || c == "Q" => (None, Action::Quit),
        _ => (None, Action::None),
    }
}

pub fn resolve_rename(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_ctrl_c(key, mods) {
        return (Some(Mode::Normal), Action::None);
    }
    match key {
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        // Return None for mode: the action handler owns the transition so it
        // can extract word_id/buffer via mem::replace before setting Mode::Normal.
        Key::Named(NamedKey::Enter) => (None, Action::RenameSubmit),
        Key::Named(NamedKey::Backspace) => (None, Action::RenameBackspace),
        Key::Character(c) => {
            if let Some(ch) = c.chars().next() {
                (None, Action::RenameChar(ch))
            } else {
                (None, Action::None)
            }
        }
        _ => (None, Action::None),
    }
}

/// Generic picker key resolver. `close`, `select`, `up`, `down`, `backspace`
/// produce the mode transition / action for those keys. `char_action` maps typed
/// characters to their picker-specific action variant.
///
/// Both Esc and Ctrl+C exit via the `close` action. Ctrl+C is mandatory as a
/// defense against text-input modes accidentally swallowing the chord.
#[allow(clippy::too_many_arguments)]
fn resolve_picker(
    key: &Key,
    mods: Modifiers,
    close: Action,
    select: Action,
    up: Action,
    down: Action,
    backspace: Action,
    char_action: fn(char) -> Action,
) -> (Option<Mode>, Action) {
    if is_ctrl_c(key, mods) {
        return (Some(Mode::Normal), close);
    }
    match key {
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), close),
        Key::Named(NamedKey::Enter) => (Some(Mode::Normal), select),
        Key::Named(NamedKey::ArrowUp) => (None, up),
        Key::Named(NamedKey::ArrowDown) => (None, down),
        Key::Named(NamedKey::Backspace) => (None, backspace),
        Key::Character(c) => {
            if let Some(ch) = c.chars().next() {
                (None, char_action(ch))
            } else {
                (None, Action::None)
            }
        }
        _ => (None, Action::None),
    }
}

pub fn resolve_session_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        mods,
        Action::CloseSessionPicker,
        Action::SelectPickerEntry,
        Action::PickerUp,
        Action::PickerDown,
        Action::PickerSearchBackspace,
        Action::PickerSearchChar,
    )
}

pub fn resolve_help(key: &Key) -> (Option<Mode>, Action) {
    // Any key exits help
    let _ = key;
    (Some(Mode::Normal), Action::None)
}

pub fn resolve_dir_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        mods,
        Action::DirPickerCancel,
        Action::DirPickerSubmit,
        Action::DirPickerUp,
        Action::DirPickerDown,
        Action::DirPickerBackspace,
        Action::DirPickerChar,
    )
}

pub fn resolve_launch_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        mods,
        Action::LaunchClose,
        Action::LaunchSelect,
        Action::LaunchUp,
        Action::LaunchDown,
        Action::LaunchSearchBackspace,
        Action::LaunchSearchChar,
    )
}

/// Esc/Ctrl+C cancels a frontend-owned launcher overlay (add-remote / remote
/// path prompt). All other input is handled by the overlay's native fields.
pub fn resolve_launch_overlay(key: &Key) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return (Some(Mode::Normal), Action::LaunchOverlayCancel);
    }
    (None, Action::None)
}

/// Esc or Ctrl+C cancels the in-progress background bootstrap.
pub fn resolve_connecting(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return (None, Action::CancelBootstrap);
    }
    if mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "c") {
        return (None, Action::CancelBootstrap);
    }
    (None, Action::None)
}

pub fn resolve_command(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    // Esc cancels and restores Normal mode. CommandState is dropped.
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return (Some(Mode::Normal), Action::None);
    }
    // Ctrl+C also cancels (familiar from shells).
    if mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "c") {
        return (Some(Mode::Normal), Action::None);
    }
    // Ctrl+U: clear the line.
    if mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "u") {
        return (None, Action::CommandClearLine);
    }
    // Ctrl+W: delete the previous word.
    if mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "w") {
        return (None, Action::CommandDeleteWordBack);
    }
    match key {
        // Submit: stay in mode so the action handler can `mem::replace` to extract
        // CommandState before transitioning to Normal.
        Key::Named(NamedKey::Enter) => (None, Action::CommandSubmit),
        Key::Named(NamedKey::Tab) => (None, Action::CommandComplete),
        Key::Named(NamedKey::Backspace) => (None, Action::CommandBackspace),
        Key::Named(NamedKey::ArrowLeft) => (None, Action::CommandLeft),
        Key::Named(NamedKey::ArrowRight) => (None, Action::CommandRight),
        Key::Named(NamedKey::ArrowUp) => (None, Action::CommandHintUp),
        Key::Named(NamedKey::ArrowDown) => (None, Action::CommandHintDown),
        Key::Named(NamedKey::Home) => (None, Action::CommandHome),
        Key::Named(NamedKey::End) => (None, Action::CommandEnd),
        Key::Character(c) => {
            if let Some(ch) = c.chars().next() {
                // Filter out the legacy Ctrl+/ byte and other control characters
                // that arrive with no CTRL modifier set — they would otherwise
                // be inserted as garbage. Only insert printable chars.
                if ch.is_control() {
                    (None, Action::None)
                } else {
                    (None, Action::CommandChar(ch))
                }
            } else {
                (None, Action::None)
            }
        }
        _ => (None, Action::None),
    }
}

#[cfg(test)]
mod tests {
    use kmux_client::key::{Key, Modifiers, NamedKey};

    use crate::mode::{Action, Mode, resolve};

    #[test]
    fn ctrl_g_enters_mode_select() {
        let (mode, action) = resolve(&Mode::Normal, &Key::Character("g".into()), Modifiers::CTRL);
        assert_eq!(mode, Some(Mode::Select));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn mode_select_s_enters_session() {
        let (mode, _) = resolve(
            &Mode::Select,
            &Key::Character("s".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Session));
    }

    #[test]
    fn session_c_creates_session() {
        let (mode, action) = resolve(
            &Mode::Session,
            &Key::Character("c".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::CreateSession);
    }

    #[test]
    fn session_p_creates_pane() {
        let (mode, action) = resolve(
            &Mode::Session,
            &Key::Character("p".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::CreatePane);
    }

    #[test]
    fn session_x_closes_pane() {
        let (_, action) = resolve(
            &Mode::Session,
            &Key::Character("x".into()),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::ClosePane);
    }

    #[test]
    fn session_shift_x_closes_session() {
        let (_, action) = resolve(
            &Mode::Session,
            &Key::Character("X".into()),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::CloseSession);
    }

    #[test]
    fn session_tab_next_pane() {
        let (_, action) = resolve(
            &Mode::Session,
            &Key::Named(NamedKey::Tab),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::NextPane);
    }

    #[test]
    fn normal_keys_forward_to_pty() {
        let (mode, action) = resolve(
            &Mode::Normal,
            &Key::Character("a".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, None);
        assert_eq!(action, Action::ForwardKey);
    }

    #[test]
    fn locked_ctrl_g_unlocks() {
        let (mode, _) = resolve(&Mode::Locked, &Key::Character("g".into()), Modifiers::CTRL);
        assert_eq!(mode, Some(Mode::Normal));
    }

    #[test]
    fn ctrl_c_cancels_session_picker() {
        let (mode, action) = resolve(
            &Mode::SessionPicker,
            &Key::Character("c".into()),
            Modifiers::CTRL,
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::CloseSessionPicker);
    }

    #[test]
    fn ctrl_c_cancels_directory_picker() {
        let (mode, action) = resolve(
            &Mode::DirectoryPicker,
            &Key::Character("c".into()),
            Modifiers::CTRL,
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::DirPickerCancel);
    }

    #[test]
    fn ctrl_c_cancels_rename_mode() {
        let (mode, _) = resolve(
            &Mode::RenameSession {
                word_id: "abc".into(),
                buffer: "x".into(),
            },
            &Key::Character("c".into()),
            Modifiers::CTRL,
        );
        assert_eq!(mode, Some(Mode::Normal));
    }

    #[test]
    fn signal_k_sends_sigkill() {
        let (mode, action) = resolve(
            &Mode::Signal,
            &Key::Character("k".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::SendSignal(9));
    }

    #[test]
    fn escape_exits_session_mode() {
        let (mode, _) = resolve(
            &Mode::Session,
            &Key::Named(NamedKey::Escape),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
    }

    #[test]
    fn session_picker_esc_closes() {
        let (mode, action) = resolve(
            &Mode::SessionPicker,
            &Key::Named(NamedKey::Escape),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::CloseSessionPicker);
    }

    #[test]
    fn session_picker_enter_selects() {
        let (mode, action) = resolve(
            &Mode::SessionPicker,
            &Key::Named(NamedKey::Enter),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::SelectPickerEntry);
    }

    // Regression tests: confirm-close and rename-submit must NOT pre-transition
    // the mode so that the action handler can extract data via mem::replace.

    #[test]
    fn confirm_close_y_does_not_change_mode() {
        let (mode, action) = resolve(
            &Mode::ConfirmCloseSession {
                word_id: "abc".into(),
            },
            &Key::Character("y".into()),
            Modifiers::empty(),
        );
        // mode must be None so the action handler can mem::replace the ConfirmCloseSession
        assert_eq!(mode, None);
        assert_eq!(action, Action::ConfirmCloseYes);
    }

    #[test]
    fn confirm_close_other_key_cancels() {
        let (mode, action) = resolve(
            &Mode::ConfirmCloseSession {
                word_id: "abc".into(),
            },
            &Key::Character("n".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn rename_enter_does_not_change_mode() {
        let (mode, action) = resolve(
            &Mode::RenameSession {
                word_id: "abc".into(),
                buffer: "new name".into(),
            },
            &Key::Named(NamedKey::Enter),
            Modifiers::empty(),
        );
        // mode must be None so the action handler can mem::replace the RenameSession
        assert_eq!(mode, None);
        assert_eq!(action, Action::RenameSubmit);
    }

    // ── Command palette ──────────────────────────────────────────────────

    fn assert_enters_command(key: Key, mods: Modifiers) {
        let (mode, action) = resolve(&Mode::Select, &key, mods);
        assert!(
            matches!(mode, Some(Mode::Command(_))),
            "expected Mode::Command, got {mode:?}"
        );
        assert_eq!(action, Action::None);
    }

    #[test]
    fn ctrl_slash_in_select_enters_command_mode() {
        assert_enters_command(Key::Character("/".into()), Modifiers::CTRL);
    }

    #[test]
    fn bare_slash_in_select_enters_command_mode() {
        assert_enters_command(Key::Character("/".into()), Modifiers::empty());
    }

    #[test]
    fn legacy_us_byte_in_select_enters_command_mode() {
        // Some terminals encode Ctrl+/ as the raw `\x1f` byte without a
        // CONTROL modifier. The activation must still fire.
        assert_enters_command(Key::Character("\u{1f}".into()), Modifiers::empty());
    }

    #[test]
    fn esc_cancels_command_mode() {
        let (mode, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Named(NamedKey::Escape),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::None);
    }

    #[test]
    fn ctrl_c_cancels_command_mode() {
        let (mode, _) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Character("c".into()),
            Modifiers::CTRL,
        );
        assert_eq!(mode, Some(Mode::Normal));
    }

    #[test]
    fn enter_in_command_mode_emits_submit_without_pre_transition() {
        let (mode, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Named(NamedKey::Enter),
            Modifiers::empty(),
        );
        // Mode must be None so the action handler can mem::replace the state.
        assert_eq!(mode, None);
        assert_eq!(action, Action::CommandSubmit);
    }

    #[test]
    fn tab_in_command_mode_emits_complete() {
        let (_, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Named(NamedKey::Tab),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::CommandComplete);
    }

    #[test]
    fn char_in_command_mode_emits_command_char() {
        let (_, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Character("a".into()),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::CommandChar('a'));
    }

    #[test]
    fn control_char_in_command_mode_does_not_insert() {
        // The legacy US byte that re-arrives mid-command must not be inserted
        // as garbage — it should be filtered.
        let (_, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Character("\u{1f}".into()),
            Modifiers::empty(),
        );
        assert_eq!(action, Action::None);
    }

    #[test]
    fn ctrl_u_clears_line_in_command_mode() {
        let (_, action) = resolve(
            &Mode::Command(crate::mode::CommandState::default()),
            &Key::Character("u".into()),
            Modifiers::CTRL,
        );
        assert_eq!(action, Action::CommandClearLine);
    }

    #[test]
    fn rename_escape_cancels() {
        let (mode, action) = resolve(
            &Mode::RenameSession {
                word_id: "abc".into(),
                buffer: String::new(),
            },
            &Key::Named(NamedKey::Escape),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::None);
    }
}
