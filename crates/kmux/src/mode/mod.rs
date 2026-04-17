mod hints;
mod resolve;

pub use hints::*;
pub use resolve::*;

use kmux_client::key::{Key, Modifiers};

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
    /// Session management: c=create session, p=create pane, X=close session,
    /// x=close pane, n=next session, Tab/j/k=pane nav, r=rename, d=disconnect
    Session,
    /// Scroll through history: Up/Down/PgUp/PgDn, Esc to exit
    Scroll,
    /// Signal menu: k=SIGKILL, t=SIGTERM, s=SIGSTOP, c=SIGCONT
    Signal,
    /// Confirm close session (y/n)
    ConfirmCloseSession { word_id: String },
    /// Rename session (typing new name)
    RenameSession { word_id: String, buffer: String },
    /// Floating session picker with search
    SessionPicker,
    /// Floating server picker with search (recent servers)
    ServerPicker,
    /// Help overlay
    Help,
    /// Connect screen (typing host/port/token)
    Connect { field: ConnectField },
    /// Directory picker for remote connections: type a path to open/create a session
    DirectoryPicker,
    /// Connection dropped. Input to panes is frozen; the overlay asks the
    /// user to confirm a reconnect.
    Disconnected { reason: String },
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

    // Pane management
    CreatePane,
    ClosePane,
    NextPane,
    PrevPane,

    // Session picker
    CloseSessionPicker,
    SelectPickerEntry,
    PickerUp,
    PickerDown,
    PickerSearchChar(char),
    PickerSearchBackspace,

    // Server picker
    ServerPickerChar(char),
    ServerPickerBackspace,
    ServerPickerUp,
    ServerPickerDown,
    ServerPickerSelect,
    ServerPickerClose,

    // Signals
    SendSignal(i32),

    // Scroll
    ScrollUp(usize),
    ScrollDown(usize),
    ScrollPageUp,
    ScrollPageDown,

    // HUD / modes
    ToggleHud,
    ToggleMetrics,
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

    // Directory picker (remote connection)
    DirPickerChar(char),
    DirPickerBackspace,
    DirPickerUp,
    DirPickerDown,
    DirPickerSubmit,
    DirPickerCancel,

    // Quit the application
    Quit,

    // Request a full reconnect via `recovery::ReconnectContext::run`.
    Reconnect,

    // No-op
    None,
}

/// The Ctrl+G mode switch key.
pub(super) fn is_mode_key(key: &Key, mods: Modifiers) -> bool {
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
        Mode::ConfirmCloseSession { .. } => resolve_confirm_close(key),
        Mode::RenameSession { .. } => resolve_rename(key, mods),
        Mode::SessionPicker => resolve_session_picker(key, mods),
        Mode::ServerPicker => resolve_server_picker(key),
        Mode::Help => resolve_help(key),
        Mode::Connect { field } => resolve_connect(key, mods, field),
        Mode::DirectoryPicker => resolve_dir_picker(key),
        Mode::Disconnected { .. } => resolve_disconnected(key),
    }
}
