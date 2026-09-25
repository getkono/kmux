//! Actions in, effects out: the verbs a frontend sends and the follow-ups
//! it is asked to perform.

use super::*;

/// What a [`KmuxDriver::tick`] / [`KmuxDriver::dispatch`] asks the frontend to
/// do. Mirrors [`FrontendEffect`]; reconnect / server-switch are handled inside
/// the driver and never surface.
#[derive(uniffi::Enum)]
pub enum FfiEffect {
    NeedsRender,
    ForceClear,
    PaletteChanged,
    CopyToClipboard {
        text: String,
    },
    RequestPaste,
    Quit,
    /// Diagnostic: rebuild the Metal renderer + glyph atlas, then repaint.
    ResetRenderer,
    /// A program in a pane requested attention via `kmux notify` (issue #169).
    /// The Swift app posts a `UNUserNotification` and, on click, refocuses the
    /// window for `word_id` and selects `pane_id`. `attention_id` dedups across
    /// the app's windows so exactly one notification is posted.
    Attention {
        word_id: String,
        pane_id: String,
        kind: FfiAttentionKind,
        title: String,
        body: String,
        attention_id: u64,
    },
}

/// Why a pane wants attention (issue #169). FFI mirror of
/// [`kmux_protocol::messages::AttentionKind`]; lets the Swift app word the
/// notification (e.g. a turn finished vs. Claude is waiting on you).
#[derive(uniffi::Enum)]
pub enum FfiAttentionKind {
    TurnDone,
    NeedsInput,
}

impl From<AttentionKind> for FfiAttentionKind {
    fn from(k: AttentionKind) -> Self {
        match k {
            AttentionKind::TurnDone => Self::TurnDone,
            AttentionKind::NeedsInput => Self::NeedsInput,
        }
    }
}

impl From<FrontendEffect> for FfiEffect {
    fn from(e: FrontendEffect) -> Self {
        match e {
            FrontendEffect::NeedsRender => Self::NeedsRender,
            FrontendEffect::ForceClear => Self::ForceClear,
            FrontendEffect::PaletteChanged => Self::PaletteChanged,
            FrontendEffect::CopyToClipboard(text) => Self::CopyToClipboard { text },
            FrontendEffect::RequestPaste => Self::RequestPaste,
            FrontendEffect::Quit => Self::Quit,
            FrontendEffect::ResetRenderer => Self::ResetRenderer,
            FrontendEffect::Attention {
                word_id,
                pane_id,
                kind,
                title,
                body,
                attention_id,
            } => Self::Attention {
                word_id,
                pane_id,
                kind: kind.into(),
                title,
                body,
                attention_id,
            },
        }
    }
}

/// A curated, toolkit-agnostic [`Action`] the frontend can dispatch by name.
/// (The full `Action` vocabulary — per-character command-palette editing, modal
/// keymap actions, … — is internal; a GUI binds widgets/accelerators to these.)
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiAction {
    CreateSession,
    CloseSession,
    NextSession,
    PrevSession,
    JumpToSession {
        index: u32,
    },
    CreatePane,
    ClosePane,
    /// Cancel the most recent soft-close within its grace window (issue #86).
    UndoClose,
    NextTab,
    PrevTab,
    /// Cycle the focused pane within the active tab (wraps at ends).
    NextPaneInTab,
    PrevPaneInTab,
    CloseTab,
    RenameTab,
    // Tiling: split the focused pane, move focus, resize the split, swap panes.
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
    SwapNext,
    SwapPrev,
    CycleLayout,
    ToggleZoom,
    /// Focus the `index`-th pane (0-based) in the active tab's leaf order.
    FocusPaneAt {
        index: u32,
    },
    ScrollUp {
        lines: u32,
    },
    ScrollDown {
        lines: u32,
    },
    ScrollPageUp,
    ScrollPageDown,
    ToggleHud,
    ToggleMetrics,
    /// Toggle the process overview main-area view (issue #122).
    ToggleProcessOverview,
    /// Toggle the connected-clients main-area view (issue #146).
    ToggleConnectedClients,
    /// Toggle the connection inspector overlay (issue #60).
    ToggleConnection,
    /// Toggle the render-debug overlay (what the renderer is handed each frame).
    ToggleRenderDebug,
    /// Rebuild the renderer + glyph atlas and full-repaint (diagnostic).
    ResetRenderer,
    ToggleInputLock,
    /// Toggle connection pause to save bandwidth (issue #68).
    TogglePause,
    /// Toggle the focused pane's exemption from *auto*-pause (issue #68): it
    /// keeps streaming when the window is backgrounded.
    ToggleFocusedPaneNoAutoPause,
    /// Toggle the active session's exemption from auto-pause (issue #68).
    ToggleActiveSessionNoAutoPause,
    CopySelection,
    Paste,
    Quit,
    Reconnect,
}

impl From<FfiAction> for Action {
    fn from(a: FfiAction) -> Self {
        match a {
            FfiAction::CreateSession => Self::CreateSession,
            FfiAction::CloseSession => Self::CloseSession,
            FfiAction::NextSession => Self::NextSession,
            FfiAction::PrevSession => Self::PrevSession,
            FfiAction::JumpToSession { index } => Self::JumpToSession(index as usize),
            FfiAction::CreatePane => Self::CreatePane,
            FfiAction::ClosePane => Self::ClosePane,
            FfiAction::UndoClose => Self::UndoClose,
            FfiAction::NextTab => Self::NextTab,
            FfiAction::PrevTab => Self::PrevTab,
            FfiAction::NextPaneInTab => Self::NextPaneInTab,
            FfiAction::PrevPaneInTab => Self::PrevPaneInTab,
            FfiAction::CloseTab => Self::CloseTab,
            FfiAction::RenameTab => Self::RenameTab,
            FfiAction::SplitRight => Self::SplitRight,
            FfiAction::SplitDown => Self::SplitDown,
            FfiAction::FocusLeft => Self::FocusLeft,
            FfiAction::FocusRight => Self::FocusRight,
            FfiAction::FocusUp => Self::FocusUp,
            FfiAction::FocusDown => Self::FocusDown,
            FfiAction::ResizeLeft => Self::ResizeLeft,
            FfiAction::ResizeRight => Self::ResizeRight,
            FfiAction::ResizeUp => Self::ResizeUp,
            FfiAction::ResizeDown => Self::ResizeDown,
            FfiAction::SwapNext => Self::SwapNext,
            FfiAction::SwapPrev => Self::SwapPrev,
            FfiAction::CycleLayout => Self::CycleLayout,
            FfiAction::ToggleZoom => Self::ToggleZoom,
            FfiAction::FocusPaneAt { index } => Self::FocusPaneAt(index),
            FfiAction::ScrollUp { lines } => Self::ScrollUp(lines as usize),
            FfiAction::ScrollDown { lines } => Self::ScrollDown(lines as usize),
            FfiAction::ScrollPageUp => Self::ScrollPageUp,
            FfiAction::ScrollPageDown => Self::ScrollPageDown,
            FfiAction::ToggleHud => Self::ToggleHud,
            FfiAction::ToggleMetrics => Self::ToggleMetrics,
            FfiAction::ToggleProcessOverview => Self::ToggleProcessOverview,
            FfiAction::ToggleConnectedClients => Self::ToggleConnectedClients,
            FfiAction::ToggleConnection => Self::ToggleConnection,
            FfiAction::ToggleRenderDebug => Self::ToggleRenderDebug,
            FfiAction::ResetRenderer => Self::ResetRenderer,
            FfiAction::ToggleInputLock => Self::ToggleInputLock,
            FfiAction::TogglePause => Self::TogglePause,
            FfiAction::ToggleFocusedPaneNoAutoPause => Self::ToggleFocusedPaneNoAutoPause,
            FfiAction::ToggleActiveSessionNoAutoPause => Self::ToggleActiveSessionNoAutoPause,
            FfiAction::CopySelection => Self::CopySelection,
            FfiAction::Paste => Self::Paste,
            FfiAction::Quit => Self::Quit,
            FfiAction::Reconnect => Self::Reconnect,
        }
    }
}

/// Connection pause state for a frontend status indicator (issue #68). Mirrors
/// [`PauseReason`].
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiPauseState {
    /// Live — not paused.
    Active,
    /// Paused by an explicit user toggle.
    PausedManual,
    /// Auto-paused because the app is backgrounded/minimized.
    PausedBackground,
}

impl From<PauseReason> for FfiPauseState {
    fn from(r: PauseReason) -> Self {
        match r {
            PauseReason::None => Self::Active,
            PauseReason::Manual => Self::PausedManual,
            PauseReason::Auto => Self::PausedBackground,
        }
    }
}
