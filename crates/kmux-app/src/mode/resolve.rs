use kmux_client::input::signal_from_key;
use kmux_client::key::{Key, Modifiers, NamedKey};

use super::{Action, Mode, is_mode_key};

/// True for the Ctrl+C cancel chord used by every text-input mode as a
/// defense-in-depth exit hatch (alongside Esc).
fn is_ctrl_c(key: &Key, mods: Modifiers) -> bool {
    mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "c")
}

/// Ctrl+Alt+R, "reconnect now".
fn is_reconnect_now(key: &Key, mods: Modifiers) -> bool {
    mods.contains(Modifiers::CTRL)
        && mods.contains(Modifiers::ALT)
        && matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("r"))
}

pub(crate) fn resolve_normal(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Select), Action::None);
    }

    // Ctrl+Alt+R: force a reconnect even without dropping first. Useful when
    // the link is degraded but has not yet tripped the liveness timeout.
    if is_reconnect_now(key, mods) {
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

pub(crate) fn resolve_locked(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_mode_key(key, mods) {
        return (Some(Mode::Normal), Action::None);
    }
    // Everything passes through in locked mode
    (None, Action::ForwardKey)
}

pub(crate) fn resolve_mode_select(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

pub(crate) fn resolve_session(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
    match key {
        Key::Character(c) => match c.as_str() {
            "c" => (Some(Mode::Normal), Action::CreateSession),
            "p" => (Some(Mode::Normal), Action::CreatePane),
            "X" => (None, Action::CloseSession),
            "x" => (Some(Mode::Normal), Action::ClosePane),
            "n" => (None, Action::NextSession),
            "j" => (None, Action::NextTab),
            "k" => (None, Action::PrevTab),
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
        Key::Named(NamedKey::Tab) => (None, Action::NextTab),
        Key::Named(NamedKey::ArrowRight) => (None, Action::NextSession),
        Key::Named(NamedKey::ArrowLeft) => (None, Action::PrevSession),
        Key::Named(NamedKey::ArrowDown) => (None, Action::NextTab),
        Key::Named(NamedKey::ArrowUp) => (None, Action::PrevTab),
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::None),
        _ => (None, Action::None),
    }
}

pub(crate) fn resolve_scroll(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

pub(crate) fn resolve_signal(key: &Key, _mods: Modifiers) -> (Option<Mode>, Action) {
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

/// Keys accepted while disconnected for good (a transient drop reconnects on
/// its own and never enters this mode): Ctrl+Alt+R reconnects now, `q` quits.
/// Everything else is dropped, so no keystroke is taken as a confirmation.
pub(crate) fn resolve_disconnected(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_reconnect_now(key, mods) {
        return (None, Action::Reconnect);
    }
    match key {
        Key::Character(c) if c == "q" || c == "Q" => (None, Action::Quit),
        _ => (None, Action::None),
    }
}

pub(crate) fn resolve_rename(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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

pub(crate) fn resolve_confirm_close_session(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if is_ctrl_c(key, mods) {
        return (Some(Mode::Normal), Action::ExitToNormal);
    }
    match key {
        Key::Named(NamedKey::Escape) => (Some(Mode::Normal), Action::ExitToNormal),
        Key::Named(NamedKey::Enter) => (None, Action::ConfirmCloseSession),
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

pub(crate) fn resolve_session_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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

pub(crate) fn resolve_help(key: &Key) -> (Option<Mode>, Action) {
    // Any key exits help
    let _ = key;
    (Some(Mode::Normal), Action::None)
}

/// Process overview (issue #122). Esc / Ctrl+G / `q` close the view; scrolling is
/// handled by the frontend's native table, so other keys are ignored here.
pub(crate) fn resolve_process_overview(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape))
        || is_mode_key(key, mods)
        || matches!(key, Key::Character(c) if c == "q")
    {
        return (None, Action::ToggleProcessOverview);
    }
    (None, Action::None)
}

/// Connected clients (issue #146). Esc / Ctrl+G / `q` close the view; the kick
/// action and scrolling are driven by the frontend's native widgets, so other
/// keys are ignored here.
pub(crate) fn resolve_connected_clients(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape))
        || is_mode_key(key, mods)
        || matches!(key, Key::Character(c) if c == "q")
    {
        return (None, Action::ToggleConnectedClients);
    }
    (None, Action::None)
}

pub(crate) fn resolve_dir_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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

pub(crate) fn resolve_launch_picker(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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
pub(crate) fn resolve_launch_overlay(key: &Key) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return (Some(Mode::Normal), Action::LaunchOverlayCancel);
    }
    (None, Action::None)
}

/// Esc or Ctrl+C cancels the in-progress background bootstrap.
pub(crate) fn resolve_connecting(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return (None, Action::CancelBootstrap);
    }
    if mods.contains(Modifiers::CTRL) && matches!(key, Key::Character(c) if c == "c") {
        return (None, Action::CancelBootstrap);
    }
    (None, Action::None)
}

pub(crate) fn resolve_command(key: &Key, mods: Modifiers) -> (Option<Mode>, Action) {
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

    use crate::mode::{Action, CommandState, Mode, resolve};

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    fn named(k: NamedKey) -> Key {
        Key::Named(k)
    }

    fn rename(buffer: &str) -> Mode {
        Mode::RenameSession {
            word_id: "abc".into(),
            buffer: buffer.into(),
        }
    }

    /// `(label, mode, key, mods, expected resolve() result)`.
    type Case = (&'static str, Mode, Key, Modifiers, (Option<Mode>, Action));

    /// One row per binding: `(label, mode, key, mods, expected (mode, action))`.
    /// Text-input submits (rename / command Enter) must resolve to `None` mode:
    /// the action handler `mem::replace`s the state out before leaving the mode.
    #[test]
    fn resolve_binding_table_yields_expected_transition_and_action() {
        let none = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        let cmd = || Mode::Command(CommandState::default());
        let to_normal = |a| (Some(Mode::Normal), a);
        let stay = |a| (None, a);
        #[rustfmt::skip]
        let cases: Vec<Case> = vec![
            ("normal ctrl+g enters select", Mode::Normal, ch("g"), ctrl, (Some(Mode::Select), Action::None)),
            ("normal plain key forwards", Mode::Normal, ch("a"), none, stay(Action::ForwardKey)),
            ("locked ctrl+g unlocks", Mode::Locked, ch("g"), ctrl, to_normal(Action::None)),
            ("select s enters session", Mode::Select, ch("s"), none, (Some(Mode::Session), Action::None)),
            ("select ctrl+/ enters command", Mode::Select, ch("/"), ctrl, (Some(cmd()), Action::None)),
            ("select bare / enters command", Mode::Select, ch("/"), none, (Some(cmd()), Action::None)),
            // Some terminals encode Ctrl+/ as the raw US byte with no CTRL modifier.
            ("select legacy US byte enters command", Mode::Select, ch("\u{1f}"), none, (Some(cmd()), Action::None)),
            ("session c creates session", Mode::Session, ch("c"), none, to_normal(Action::CreateSession)),
            ("session p creates pane", Mode::Session, ch("p"), none, to_normal(Action::CreatePane)),
            ("session x closes pane", Mode::Session, ch("x"), none, to_normal(Action::ClosePane)),
            ("session X closes session", Mode::Session, ch("X"), none, stay(Action::CloseSession)),
            ("session tab next tab", Mode::Session, named(NamedKey::Tab), none, stay(Action::NextTab)),
            ("session esc exits", Mode::Session, named(NamedKey::Escape), none, to_normal(Action::None)),
            ("signal k sends sigkill", Mode::Signal, ch("k"), none, to_normal(Action::SendSignal(9))),
            ("session picker ctrl+c closes", Mode::SessionPicker, ch("c"), ctrl, to_normal(Action::CloseSessionPicker)),
            ("session picker esc closes", Mode::SessionPicker, named(NamedKey::Escape), none, to_normal(Action::CloseSessionPicker)),
            ("session picker enter selects", Mode::SessionPicker, named(NamedKey::Enter), none, to_normal(Action::SelectPickerEntry)),
            ("dir picker ctrl+c cancels", Mode::DirectoryPicker, ch("c"), ctrl, to_normal(Action::DirPickerCancel)),
            ("rename ctrl+c cancels", rename("x"), ch("c"), ctrl, to_normal(Action::None)),
            ("rename esc cancels", rename(""), named(NamedKey::Escape), none, to_normal(Action::None)),
            ("rename enter submits in place", rename("new name"), named(NamedKey::Enter), none, stay(Action::RenameSubmit)),
            ("command esc cancels", cmd(), named(NamedKey::Escape), none, to_normal(Action::None)),
            ("command ctrl+c cancels", cmd(), ch("c"), ctrl, to_normal(Action::None)),
            ("command enter submits in place", cmd(), named(NamedKey::Enter), none, stay(Action::CommandSubmit)),
            ("command tab completes", cmd(), named(NamedKey::Tab), none, stay(Action::CommandComplete)),
            ("command char inserts", cmd(), ch("a"), none, stay(Action::CommandChar('a'))),
            // The legacy US byte re-arriving mid-command must not insert garbage.
            ("command control char filtered", cmd(), ch("\u{1f}"), none, stay(Action::None)),
            ("command ctrl+u clears line", cmd(), ch("u"), ctrl, stay(Action::CommandClearLine)),
        ];
        for (label, mode, key, mods, expected) in cases {
            assert_eq!(resolve(&mode, &key, mods), expected, "{label}");
        }
    }

    /// No keystroke confirms a reconnect any more (issue #208): `y` and Enter
    /// do nothing while disconnected; Ctrl+Alt+R reconnects now and `q`
    /// quits.
    #[test]
    fn disconnected_takes_only_reconnect_now_and_quit() {
        let disconnected = Mode::Disconnected {
            reason: "auth failed".into(),
        };
        let none = Modifiers::empty();
        for key in [
            Key::Character("y".into()),
            Key::Character("Y".into()),
            Key::Named(NamedKey::Enter),
            Key::Character("r".into()),
        ] {
            assert_eq!(resolve(&disconnected, &key, none), (None, Action::None));
        }
        for half_chord in [Modifiers::CTRL, Modifiers::ALT] {
            assert_eq!(
                resolve(&disconnected, &Key::Character("r".into()), half_chord),
                (None, Action::None),
                "Ctrl+Alt+R takes both modifiers"
            );
        }
        assert_eq!(
            resolve(
                &disconnected,
                &Key::Character("r".into()),
                Modifiers::CTRL | Modifiers::ALT
            ),
            (None, Action::Reconnect)
        );
        assert_eq!(
            resolve(&disconnected, &Key::Character("q".into()), none),
            (None, Action::Quit)
        );
    }
}
