use kmux_client::input::signal_from_key;
use kmux_client::key::{Key, Modifiers, NamedKey};

use super::{Action, ConnectField, Mode, is_mode_key};

pub fn resolve_normal(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Select), Action::None);
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
    match key {
        Key::Character(c) => match c.as_str() {
            "s" => (Some(Mode::Session), Action::None),
            "o" => (Some(Mode::Scroll), Action::None),
            "k" => (Some(Mode::Signal), Action::None),
            "l" => (Some(Mode::Locked), Action::None),
            "h" => (Some(Mode::Normal), Action::ToggleHud),
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
        Key::Named(NamedKey::Escape) | Key::Character(_) if matches!(key, Key::Character(c) if c == "q") => {
            (Some(Mode::Normal), Action::ExitToNormal)
        }
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::ExitToNormal),
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
        Key::Character(c) if c == "y" => (Some(Mode::Normal), Action::ConfirmCloseYes),
        _ => (Some(Mode::Normal), Action::None),
    }
}

pub fn resolve_rename(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        Key::Named(NamedKey::Enter) => (Some(Mode::Normal), Action::RenameSubmit),
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
fn resolve_picker(
    key: &Key,
    close: Action,
    select: Action,
    up: Action,
    down: Action,
    backspace: Action,
    char_action: fn(char) -> Action,
) -> (Option<Mode>, Action) {
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

pub fn resolve_session_picker(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        Action::CloseSessionPicker,
        Action::SelectPickerEntry,
        Action::PickerUp,
        Action::PickerDown,
        Action::PickerSearchBackspace,
        Action::PickerSearchChar,
    )
}

pub fn resolve_server_picker(key: &Key) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        Action::ServerPickerClose,
        Action::ServerPickerSelect,
        Action::ServerPickerUp,
        Action::ServerPickerDown,
        Action::ServerPickerBackspace,
        Action::ServerPickerChar,
    )
}

pub fn resolve_help(key: &Key) -> (Option<Mode>, Action) {
    // Any key exits help
    let _ = key;
    (Some(Mode::Normal), Action::None)
}

pub fn resolve_dir_picker(key: &Key) -> (Option<Mode>, Action) {
    resolve_picker(
        key,
        Action::DirPickerCancel,
        Action::DirPickerSubmit,
        Action::DirPickerUp,
        Action::DirPickerDown,
        Action::DirPickerBackspace,
        Action::DirPickerChar,
    )
}

pub fn resolve_connect(
    key: &Key,
    mods: Modifiers,
    _field: &ConnectField,
) -> (Option<Mode>, Action) {
    match key {
        Key::Named(NamedKey::Enter) => (None, Action::ConnectSubmit),
        Key::Named(NamedKey::Tab) => {
            if mods.contains(Modifiers::SHIFT) {
                (None, Action::ConnectPrevField)
            } else {
                (None, Action::ConnectNextField)
            }
        }
        Key::Named(NamedKey::Backspace) => (None, Action::ConnectBackspace),
        Key::Character(c) => {
            if let Some(ch) = c.chars().next() {
                (None, Action::ConnectChar(ch))
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
}
