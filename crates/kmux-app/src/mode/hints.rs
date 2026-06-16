use super::Mode;

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
            ("m", "Metrics"),
            ("r", "Redraw"),
            ("?", "Help"),
            ("q", "Quit"),
            ("Esc", "Cancel"),
        ],
        Mode::Session => vec![
            ("c", "New session"),
            ("p", "New pane"),
            ("X", "Close session"),
            ("x", "Close pane"),
            ("n/\u{2190}\u{2192}", "Sessions"),
            ("Tab/j/k", "Panes"),
            ("r", "Rename"),
            ("d", "Disconnect"),
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
        Mode::ConfirmCloseSession { .. } => vec![("y", "Confirm close"), ("any", "Cancel")],
        Mode::RenameSession { .. } | Mode::RenameTab { .. } => {
            vec![("Enter", "Submit"), ("Esc", "Cancel")]
        }
        Mode::SessionPicker => vec![
            ("\u{2191}/\u{2193}", "Navigate"),
            ("Enter", "Select"),
            ("Esc", "Cancel"),
        ],
        Mode::ServerPicker => vec![
            ("\u{2191}/\u{2193}", "Navigate"),
            ("Enter", "Connect"),
            ("Esc", "Cancel"),
        ],
        Mode::Help => vec![("any key", "Close")],
        Mode::DirectoryPicker => vec![
            ("\u{2191}/\u{2193}", "Navigate"),
            ("Enter", "Open/create"),
            ("Esc", "Cancel"),
        ],
        Mode::LaunchPicker => vec![
            ("\u{2191}/\u{2193}", "Navigate"),
            ("Enter", "Open/expand"),
            ("Esc", "Cancel"),
        ],
        Mode::AddRemote => vec![("Enter", "Add"), ("Esc", "Cancel")],
        Mode::RemoteNewSession { .. } => vec![("Enter", "Create"), ("Esc", "Cancel")],
        Mode::Connecting { .. } => vec![("Esc", "Cancel")],
        Mode::Disconnected { .. } => vec![("y/Enter", "Reconnect"), ("q", "Quit")],
        Mode::Command(_) => vec![
            ("Tab", "Complete"),
            ("\u{2191}/\u{2193}", "Hint"),
            ("Enter", "Run"),
            ("Esc", "Cancel"),
        ],
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
        Mode::ConfirmCloseSession { .. } => "CONFIRM CLOSE",
        Mode::RenameSession { .. } => "RENAME",
        Mode::RenameTab { .. } => "RENAME TAB",
        Mode::SessionPicker => "SESSION PICKER",
        Mode::ServerPicker => "SERVER PICKER",
        Mode::Help => "HELP",
        Mode::DirectoryPicker => "OPEN SESSION",
        Mode::LaunchPicker => "LAUNCHER",
        Mode::AddRemote => "ADD REMOTE",
        Mode::RemoteNewSession { .. } => "NEW REMOTE SESSION",
        Mode::Connecting { .. } => "CONNECTING",
        Mode::Disconnected { .. } => "DISCONNECTED",
        Mode::Command(_) => "COMMAND",
    }
}

/// The accent color a mode is badged with, as a toolkit-neutral [`Rgb`] drawn
/// from the active palette. Each frontend converts the result to its own color
/// type at the render boundary (the GTK GUI to a cairo/CSS color), so the
/// mode→color mapping has one source of truth.
///
/// [`Rgb`]: crate::theme::Rgb
pub fn mode_rgb(mode: &Mode, theme: &crate::theme::Theme) -> crate::theme::Rgb {
    match mode {
        Mode::Normal => theme.green,
        Mode::Locked => theme.red,
        Mode::Select => theme.accent,
        Mode::Session => theme.purple,
        Mode::Scroll => theme.yellow,
        Mode::Signal => theme.red,
        Mode::ConfirmCloseSession { .. } => theme.red,
        Mode::RenameSession { .. } | Mode::RenameTab { .. } => theme.orange,
        Mode::SessionPicker => theme.accent,
        Mode::ServerPicker => theme.purple,
        Mode::Help => theme.accent,
        Mode::DirectoryPicker => theme.accent,
        Mode::LaunchPicker => theme.accent,
        Mode::AddRemote => theme.purple,
        Mode::RemoteNewSession { .. } => theme.accent,
        Mode::Connecting { .. } => theme.yellow,
        Mode::Disconnected { .. } => theme.red,
        Mode::Command(_) => theme.accent,
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
        ("m", "Toggle metrics overlay"),
        ("r", "Force redraw"),
        ("?", "This help"),
        ("q", "Quit"),
        ("", ""),
        ("-- Session Mode --", ""),
        ("c", "Create new session"),
        ("p", "Create new pane"),
        ("X", "Close current session"),
        ("x", "Close current pane"),
        ("n / \u{2192}", "Next session"),
        ("\u{2190}", "Previous session"),
        ("Tab / j", "Next pane"),
        ("k", "Previous pane"),
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
        ("Ctrl+Alt+R", "Force reconnect"),
    ]
}
