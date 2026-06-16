mod hints;
mod resolve;

pub use hints::*;
pub use resolve::*;

use kmux_client::key::{Key, Modifiers};

/// Zellij-style interaction mode (the modal keymap available to frontends).
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
    /// Rename a tab (typing new name)
    RenameTab {
        word_id: String,
        tab_index: u32,
        buffer: String,
    },
    /// Floating session picker with search
    SessionPicker,
    /// Help overlay
    Help,
    /// Directory picker for remote connections: type a path to open/create a session
    DirectoryPicker,
    /// Unified session launcher (issue #121): a searchable list to open or create
    /// a session locally or on a remote. Rows come from [`AppCore::launch_rows`].
    LaunchPicker,
    /// Add-a-remote form (issue #121). The frontend owns the native input fields
    /// and calls [`AppCore::submit_add_remote`]; the core only opens/cancels.
    AddRemote,
    /// Prompt for the directory of a new session on a federated `peer` (issue
    /// #121). The frontend owns the path field and calls
    /// [`AppCore::submit_remote_new_session`].
    RemoteNewSession { peer: String },
    /// Background bootstrap in progress. Input is held; Esc cancels.
    Connecting { target_display: String },
    /// Connection dropped. Input to panes is frozen; the overlay asks the
    /// user to confirm a reconnect.
    Disconnected { reason: String },
    /// Command palette: type `/<command> [args]` with autocomplete hints.
    /// Activated by Ctrl+G then Ctrl+/ (or `/`).
    Command(CommandState),
}

/// Editing state for `Mode::Command`. The leading `/` is rendered as static
/// chrome by the overlay and is **not** stored in `buffer`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandState {
    /// Raw command text, without the leading `/`.
    pub buffer: String,
    /// Byte offset of the caret within `buffer`.
    pub cursor: usize,
    /// Index of the highlighted hint in the dropdown.
    pub selected: usize,
    /// `Some(i)` when the buffer was recalled from history; `None` while
    /// freely editing. Lets ↑/↓ scroll history without losing the in-progress
    /// buffer if the user changes their mind.
    pub history_pos: Option<usize>,
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

    // Pane / tab management.
    // (`CreatePane`/`NextPane`/`PrevPane` map to tab operations — the
    // user-facing "pane" the chrome shows as a tab is now a Tab.)
    CreatePane,
    ClosePane,
    /// Cancel the most recent soft-close before its grace window elapses (#86).
    UndoClose,
    NextPane,
    PrevPane,
    CloseTab,
    RenameTab,

    // Tiling: split the focused pane, move focus between tiled panes, and resize
    // the focused pane's enclosing split.
    SplitRight,
    SplitDown,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    /// Move the focused pane forward/back in the tab's leaf order (focus follows
    /// the moved pane to its new slot).
    SwapNext,
    SwapPrev,
    /// Regenerate the active tab into the next preset layout (tmux next-layout).
    CycleLayout,
    /// Toggle tmux-style zoom of the focused pane (client-local view flag).
    ToggleZoom,
    /// Focus the `i`-th pane (0-based) in the active tab's leaf order.
    FocusPaneAt(u32),

    // Session picker
    CloseSessionPicker,
    SelectPickerEntry,
    PickerUp,
    PickerDown,
    PickerSearchChar(char),
    PickerSearchBackspace,

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
    /// Toggle the connection inspector overlay (issue #60).
    ToggleConnection,
    ToggleSnapshotMode,
    ToggleInputLock,
    /// Toggle connection pause to save bandwidth (issue #68). Manual pauses
    /// persist across window focus changes (auto-pause clears independently).
    TogglePause,

    // Clipboard
    CopySelection,
    Paste,

    // Input
    ForwardKey,

    // Mode transitions
    ExitToNormal,

    // Directory picker (remote connection)
    DirPickerChar(char),
    DirPickerBackspace,
    DirPickerUp,
    DirPickerDown,
    DirPickerSubmit,
    DirPickerCancel,

    // Unified launcher (issue #121)
    LaunchClose,
    LaunchSelect,
    LaunchUp,
    LaunchDown,
    LaunchSearchChar(char),
    LaunchSearchBackspace,
    /// Cancel a frontend-owned launcher overlay (add-remote / remote path prompt).
    LaunchOverlayCancel,

    // Cancel an in-progress background bootstrap.
    CancelBootstrap,

    // Command palette editing
    CommandChar(char),
    CommandBackspace,
    CommandLeft,
    CommandRight,
    CommandHome,
    CommandEnd,
    CommandHintUp,
    CommandHintDown,
    CommandComplete,
    CommandSubmit,
    CommandClearLine,
    CommandDeleteWordBack,

    // Quit the application
    Quit,

    // Request a full reconnect via `recovery::ReconnectContext::run`.
    Reconnect,

    // Force a full terminal clear + redraw (diagnostic).
    ForceRedraw,

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
        Mode::RenameSession { .. } | Mode::RenameTab { .. } => resolve_rename(key, mods),
        Mode::SessionPicker => resolve_session_picker(key, mods),
        Mode::Help => resolve_help(key),
        Mode::DirectoryPicker => resolve_dir_picker(key, mods),
        Mode::LaunchPicker => resolve_launch_picker(key, mods),
        Mode::AddRemote | Mode::RemoteNewSession { .. } => resolve_launch_overlay(key),
        Mode::Connecting { .. } => resolve_connecting(key, mods),
        Mode::Disconnected { .. } => resolve_disconnected(key),
        Mode::Command(_) => resolve_command(key, mods),
    }
}
