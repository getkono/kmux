use kmux_client::key::{Key, Modifiers, NamedKey};

/// Zellij-style mode for the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Mode {
    /// Keys pass through to PTY. Only Ctrl+G is intercepted.
    #[default]
    Normal,
    /// Everything passes through. Ctrl+G to unlock.
    Locked,
    /// Mode selector shown at bottom. Single key picks a mode.
    Select,
    /// Session management: c=create, x=close, n/p=next/prev, 0-9=jump, r=rename, d=disconnect
    Session,
    /// Scroll through history: Up/Down/PgUp/PgDn, Esc to exit
    Scroll,
    /// Signal menu: k=SIGKILL, t=SIGTERM, s=SIGSTOP, c=SIGCONT
    Signal,
    /// Confirm close session (y/n)
    ConfirmClose { session: String },
    /// Rename session (typing new name)
    Rename { session: String, buffer: String },
    /// Help overlay
    Help,
    /// Connect screen (typing host/port/token)
    Connect { field: ConnectField },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectField {
    Host,
    Port,
    Token,
}

/// Actions that the app should perform in response to key input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    // Session management
    CreateSession,
    CloseSession,
    ConfirmCloseYes,
    NextSession,
    PrevSession,
    JumpToSession(usize),
    RenameSession,
    RenameChar(char),
    RenameBackspace,
    RenameSubmit,
    Disconnect,

    // Signals
    SendSignal(i32),

    // Scroll
    ScrollUp(usize),
    ScrollDown(usize),
    ScrollPageUp,
    ScrollPageDown,

    // HUD / modes
    ToggleHud,
    ToggleSnapshotMode,
    ToggleInputLock,

    // Clipboard
    CopySelection,
    Paste,

    // Input
    ForwardKey,

    // Mode transitions
    ExitToNormal,

    // Connect screen
    ConnectSubmit,
    ConnectNextField,
    ConnectPrevField,
    ConnectChar(char),
    ConnectBackspace,

    // Quit the application
    Quit,

    // No-op
    None,
}

/// The Ctrl+G mode switch key.
fn is_mode_key(key: &Key, mods: Modifiers) -> bool {
    mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "g")
}

/// Resolve a key press in the current mode into a (new_mode, action) pair.
pub fn resolve(mode: &Mode, key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    match mode {
        Mode::Normal => resolve_normal(key, mods),
        Mode::Locked => resolve_locked(key, mods),
        Mode::Select => resolve_mode_select(key, mods),
        Mode::Session => resolve_session(key, mods),
        Mode::Scroll => resolve_scroll(key, mods),
        Mode::Signal => resolve_signal(key, mods),
        Mode::ConfirmClose { .. } => resolve_confirm_close(key),
        Mode::Rename { .. } => resolve_rename(key, mods),
        Mode::Help => resolve_help(key),
        Mode::Connect { field } => resolve_connect(key, mods, field),
    }
}

fn resolve_normal(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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

fn resolve_locked(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Normal), Action::None);
    }
    // Everything passes through in locked mode
    (None, Action::ForwardKey)
}

fn resolve_mode_select(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

fn resolve_session(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) => match c.as_str() {
            "c" => (Some(Mode::Normal), Action::CreateSession),
            "x" => (None, Action::CloseSession),
            "n" => (None, Action::NextSession),
            "p" => (None, Action::PrevSession),
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
        Key::Named(NamedKey::ArrowRight) => (None, Action::NextSession),
        Key::Named(NamedKey::ArrowLeft) => (None, Action::PrevSession),
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (None, Action::None),
    }
}

fn resolve_scroll(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

fn resolve_signal(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) => {
            let action = match c.as_str() {
                "k" => Action::SendSignal(9),  // SIGKILL
                "t" => Action::SendSignal(15), // SIGTERM
                "s" => Action::SendSignal(19), // SIGSTOP
                "c" => Action::SendSignal(18), // SIGCONT
                _ => Action::None,
            };
            (Some(Mode::Normal), action)
        }
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (None, Action::None),
    }
}

fn resolve_confirm_close(key: &Key) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) if c == "y" => (Some(Mode::Normal), Action::ConfirmCloseYes),
        _ => (Some(Mode::Normal), Action::None),
    }
}

fn resolve_rename(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

fn resolve_help(key: &Key) -> (Option<Mode>, Action) {
    // Any key exits help
    let _ = key;
    (Some(Mode::Normal), Action::None)
}

fn resolve_connect(key: &Key, mods: Modifiers, _field: &ConnectField) -> (Option<Mode>, Action) {
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

/// Returns hint bar entries for a given mode: (key_label, description).
pub fn mode_hints(mode: &Mode) -> Vec<(&'static str, &'static str)> {
    match mode {
        Mode::Normal => vec![("Ctrl+G", "Mode select")],
        Mode::Locked => vec![("Ctrl+G", "Unlock")],
        Mode::Select => vec![
            ("s", "Session"),
            ("o", "Scroll"),
            ("k", "Signal"),
            ("l", "Lock"),
            ("h", "HUD"),
            ("?", "Help"),
            ("q", "Quit"),
            ("Esc", "Cancel"),
        ],
        Mode::Session => vec![
            ("c", "Create"),
            ("x", "Close"),
            ("n/p", "Next/Prev"),
            ("r", "Rename"),
            ("d", "Disconnect"),
            ("l", "Lock input"),
            ("f", "Snapshot"),
            ("0-9", "Jump"),
            ("Esc", "Back"),
        ],
        Mode::Scroll => vec![
            ("\u{2191}/\u{2193}", "Scroll"),
            ("PgUp/Dn", "Page"),
            ("q/Esc", "Exit"),
        ],
        Mode::Signal => vec![
            ("k", "SIGKILL"),
            ("t", "SIGTERM"),
            ("s", "SIGSTOP"),
            ("c", "SIGCONT"),
            ("Esc", "Cancel"),
        ],
        Mode::ConfirmClose { session: _ } => vec![("y", "Confirm close"), ("any", "Cancel")],
        Mode::Rename { .. } => vec![("Enter", "Submit"), ("Esc", "Cancel")],
        Mode::Help => vec![("any key", "Close")],
        Mode::Connect { .. } => vec![("Tab", "Next field"), ("Enter", "Connect")],
    }
}

/// Display name for the mode (shown in status bar).
pub fn mode_name(mode: &Mode) -> &'static str {
    match mode {
        Mode::Normal => "NORMAL",
        Mode::Locked => "LOCKED",
        Mode::Select => "SELECT MODE",
        Mode::Session => "SESSION",
        Mode::Scroll => "SCROLL",
        Mode::Signal => "SIGNAL",
        Mode::ConfirmClose { .. } => "CONFIRM CLOSE",
        Mode::Rename { .. } => "RENAME",
        Mode::Help => "HELP",
        Mode::Connect { .. } => "CONNECT",
    }
}

/// Help entries for the full help overlay.
pub fn help_entries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Ctrl+G", "Enter mode selector"),
        ("", ""),
        ("-- Mode Select --", ""),
        ("s", "Session mode"),
        ("o", "Scroll mode"),
        ("k", "Signal mode"),
        ("l", "Locked mode (passthrough)"),
        ("h", "Toggle HUD metrics"),
        ("?", "This help"),
        ("q", "Quit"),
        ("", ""),
        ("-- Session Mode --", ""),
        ("c", "Create new session"),
        ("x", "Close current session"),
        ("n / \u{2192}", "Next session"),
        ("p / \u{2190}", "Previous session"),
        ("0-9", "Jump to session"),
        ("r", "Rename session"),
        ("d", "Disconnect"),
        ("l", "Toggle input lock"),
        ("f", "Toggle snapshot mode"),
        ("", ""),
        ("-- Scroll Mode --", ""),
        ("\u{2191}/\u{2193}", "Scroll line"),
        ("PgUp/PgDn", "Scroll page"),
        ("q / Esc", "Exit scroll"),
        ("", ""),
        ("-- Global --", ""),
        ("Shift+PgUp/Dn", "Quick scroll"),
        ("Ctrl+Shift+C", "Copy selection"),
        ("Ctrl+Shift+V", "Paste"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn session_c_creates() {
        let (mode, action) = resolve(
            &Mode::Session,
            &Key::Character("c".into()),
            Modifiers::empty(),
        );
        assert_eq!(mode, Some(Mode::Normal));
        assert_eq!(action, Action::CreateSession);
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
}
