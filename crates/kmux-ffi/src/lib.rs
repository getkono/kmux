//! uniffi bindings exposing the kmux client to non-Rust frontends.
//!
//! This crate is the language boundary for a native GUI written in another
//! language — concretely, the SwiftUI macOS app. It wraps the toolkit-agnostic
//! [`kmux_app::driver::FrontendDriver`] in an opaque, thread-confined
//! [`KmuxDriver`] handle and re-exports a small, flat, uniffi-friendly surface:
//!
//! - **Lifecycle**: [`KmuxDriver::new`] builds an `AppCore` (resolving the
//!   server target / theme exactly as the CLI front door does), wraps it in a
//!   `FrontendDriver`, and owns the tokio runtime the driver's background tasks
//!   run on.
//! - **Pump**: [`KmuxDriver::tick`] runs one driver iteration and returns the
//!   [`FfiEffect`]s the frontend must act on. The Swift app calls this each frame
//!   (a main-thread timer, the analog of the GTK `glib` timeout).
//! - **Render (hot path)**: [`KmuxDriver::grid_info`] /
//!   [`KmuxDriver::grid_snapshot`] expose the active grid as a generation-gated,
//!   packed byte buffer (see [`kmux_render::packed`]) so the renderer copies only changed
//!   frames; plus [`KmuxDriver::theme`], [`KmuxDriver::blink_on`],
//!   [`KmuxDriver::selection`], and [`KmuxDriver::scroll_info`].
//! - **Input**: structured **mode-aware** keys ([`KmuxDriver::send_char`] /
//!   [`KmuxDriver::send_named_key`], encoded by the daemon — not hand-rolled
//!   here), [`KmuxDriver::dispatch`] (a curated [`FfiAction`] set),
//!   [`KmuxDriver::send_input`] (raw PTY bytes), [`KmuxDriver::feed_paste`], the
//!   mouse helpers ([`KmuxDriver::scroll_at`] + the selection setters), and
//!   [`KmuxDriver::set_term_size`] / [`KmuxDriver::request_resize`].
//! - **Chrome / overlays**: [`KmuxDriver::connection`], [`KmuxDriver::sessions`],
//!   [`KmuxDriver::panes`] / [`KmuxDriver::select_pane`], [`KmuxDriver::mode`],
//!   the command palette ([`KmuxDriver::command_hints`] /
//!   [`KmuxDriver::run_command`]), the [`KmuxDriver::picker`] getter + drivers,
//!   session [`KmuxDriver::rename_session`] / [`KmuxDriver::close_session`],
//!   [`KmuxDriver::metrics`], and [`KmuxDriver::available_themes`] /
//!   [`KmuxDriver::set_theme`].
//!
//! ## Threading
//!
//! All `KmuxDriver` methods are expected to be called from a single thread (the
//! Swift main thread). Background tokio tasks communicate with the driver only
//! through the four mpsc channels it owns; the `Mutex<FrontendDriver>` makes the
//! handle `Send + Sync` for uniffi, and `tick` (under the lock) is the sole
//! place `AppCore` is mutated, so there is no shared mutable cross-thread
//! access. Off-main-thread calls are not part of the contract yet.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use tokio::runtime::Runtime;

use kmux_app::appearance::{Appearance, CellAdjust};
use kmux_app::cmd;
use kmux_app::config;
use kmux_app::core::{
    AddRemoteForm, AppCore, DirBrowserRow, LaunchRow, OverviewRowKind, PauseReason, RemoteStatus,
    TopBarAction,
};
use kmux_app::diagnostic::{self, DiagnosticTest};
use kmux_app::driver::{FrontendDriver, FrontendEffect};
use kmux_app::mode::{Action, CommandState, Mode};
use kmux_app::subcommands::parse_target;
use kmux_app::theme::{self, Rgb, Theme};
use kmux_client::connection_state::ConnectionState;
use kmux_client::generate_instance_id;
use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::{
    MouseButton, MouseEvent, MouseEventKind, MouseMods, char_to_proto_key, encode_mouse_scroll,
};
use kmux_protocol::messages::{
    AttentionKind, ClientCapabilities, KeyAction, KeyCode, KeyEvent, KeyMods, PaneProgressState,
    SplitDir, TermSize,
};
use kmux_protocol::{format_pane_id, pane_index};
// The packed-cell format is owned by `kmux-render` (the single encoder shared
// with the GPU renderer; see docs/architecture-render.md). The non-GPU Swift
// path encodes through it here, so the bytes are identical to the renderer's.
use kmux_render::packed;
// `CursorView` is part of kmux-render's wgpu-free core and the render-debug
// overlay reads it on the Cairo path too, so it must not sit behind `gpu` — the
// lean build (`mise run build-no-gpu`) needs it.
use kmux_render::CursorView;
#[cfg(feature = "gpu")]
use kmux_render::{CellSource, Frame, PaneView, ScrollIndicator, TerminalRenderer};

uniffi::setup_scaffolding!();

/// ABI version of this FFI surface. Bumped on any breaking change to the
/// exported types/functions, mirroring the repo's other versioned boundaries
/// (`kmux-ghostty-sys`'s `EXPECTED_ABI_VERSION`, the wire protocol range).
/// The Swift wrapper asserts this on startup, on top of uniffi's built-in
/// binding-checksum check.
pub const KMUX_FFI_ABI_VERSION: u32 = 25;

/// Returns [`KMUX_FFI_ABI_VERSION`]. A free function so the Swift wrapper can
/// check it before constructing a driver.
#[uniffi::export]
pub fn kmux_ffi_abi_version() -> u32 {
    KMUX_FFI_ABI_VERSION
}

/// Build + version metadata for the Swift app's "About" panel — the same matrix
/// `kmux -V` prints, plus this binary's renderer API and FFI ABI versions. Mirror
/// of [`kmux_app::version::VersionInfo`] flattened for uniffi.
#[derive(uniffi::Record)]
pub struct FfiVersionInfo {
    /// Crate semver, e.g. `"0.2.0"`.
    pub semver: String,
    /// Short git commit (or `"unknown"`).
    pub git_sha: String,
    /// Working tree had uncommitted changes at build time.
    pub git_dirty: bool,
    /// Build date, `YYYY-MM-DD`.
    pub build_date: String,
    /// Cargo profile (`"debug"` / `"release"`).
    pub build_profile: String,
    /// Client↔daemon supported wire protocol range.
    pub protocol: String,
    /// Renderer API version (this binary links `kmux-render`).
    pub render_api: u32,
    /// FFI C-ABI version ([`KMUX_FFI_ABI_VERSION`]).
    pub ffi_abi: u32,
}

/// Version + build metadata for the Swift "About" panel. The build identity
/// (semver, git SHA + dirty, date, profile) is what makes a running build
/// verifiable; the protocol/render/FFI numbers pin the linked boundaries.
#[uniffi::export]
pub fn kmux_ffi_version_info() -> FfiVersionInfo {
    let v = kmux_app::version::VersionInfo::current();
    FfiVersionInfo {
        semver: v.semver.to_string(),
        git_sha: v.git_sha.to_string(),
        git_dirty: v.git_dirty,
        build_date: v.build_date.to_string(),
        build_profile: v.build_profile.to_string(),
        protocol: v.protocol,
        render_api: kmux_render::KMUX_RENDER_API_VERSION,
        ffi_abi: KMUX_FFI_ABI_VERSION,
    }
}

/// The terminal renderer backend resolved from `~/.config/kmux/config.toml`
/// (`"cairo"` or `"gpu"`), as a stable lowercase token. The Swift frontend calls
/// this once at startup to decide whether to build the Metal path — the renderer
/// is config-only (not a CLI flag) because a singleton GUI client cannot honor a
/// per-launch flag. The render-debug overlay still reports the *effective*
/// renderer from live state (a GPU-init failure falls back to CoreText).
#[uniffi::export]
pub fn resolve_renderer() -> String {
    config::resolve_renderer().as_str().to_string()
}

/// The bytes of the bundled symbol fallback font (Symbols Nerd Font Mono). The
/// Swift app registers these with CoreText at startup
/// (`CTFontManagerRegisterGraphicsFont`) so its CoreText render path — and any
/// Nerd-glyph icons in the app chrome — resolve Powerline/PUA glyphs the
/// configured font lacks, matching the GPU atlas's fallback (issue #145).
#[uniffi::export]
pub fn symbol_fallback_font_bytes() -> Vec<u8> {
    kmux_render::symbol_fallback_bytes().to_vec()
}

/// The kmux-render API version this crate was written against. A compile-time
/// guard (below) asserts the linked `kmux-render` matches, so the renderer
/// boundary is versioned like the others.
#[cfg(feature = "gpu")]
pub const EXPECTED_RENDER_API: u32 = 1;

#[cfg(feature = "gpu")]
const _: () = assert!(
    kmux_render::KMUX_RENDER_API_VERSION == EXPECTED_RENDER_API,
    "linked kmux-render API version does not match EXPECTED_RENDER_API",
);

/// The linked kmux-render API version, asserted on the Swift side alongside
/// [`KMUX_FFI_ABI_VERSION`].
#[cfg(feature = "gpu")]
#[uniffi::export]
pub fn kmux_ffi_render_api_version() -> u32 {
    kmux_render::KMUX_RENDER_API_VERSION
}

/// Failure constructing a [`KmuxDriver`].
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("failed to initialize kmux: {message}")]
    Init { message: String },
    /// GPU renderer construction or operation failed (the `gpu` feature).
    #[error("renderer error: {message}")]
    Render { message: String },
}

/// Parameters for [`KmuxDriver::new`]. Mirrors the CLI front door's launch plan:
/// `server` is `None` for the local daemon or `"[user@]host"` for SSH; `theme`
/// is a built-in theme name (defaulting when `None`); `rows`/`cols` and the
/// pixel size are the initial content geometry.
#[derive(uniffi::Record)]
pub struct DriverConfig {
    pub server: Option<String>,
    pub ssh_port: Option<u16>,
    pub cwd: Option<String>,
    pub session: Option<String>,
    pub theme: Option<String>,
    /// Whether the inner-pane cursor blinks. `None` defaults to `true` (and
    /// falls back to the `cursor_blink` key in `config.toml`).
    pub cursor_blink: Option<bool>,
    /// Render diagnostic to launch instead of a shell session: a
    /// [`DiagnosticTest`] name
    /// (`glyphs`/`attrs`/…). `None` for an ordinary launch (issue #145).
    pub diagnostic: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

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
#[derive(uniffi::Enum)]
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

/// Keyboard modifier state for a structured key event. Maps to [`KeyMods`].
#[derive(uniffi::Record)]
pub struct FfiKeyMods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// The Command (⌘) key on macOS; maps to `KeyMods::SUPER`.
    pub command: bool,
}

impl FfiKeyMods {
    fn to_proto(&self) -> KeyMods {
        let mut m = KeyMods::empty();
        m.set(KeyMods::SHIFT, self.shift);
        m.set(KeyMods::CTRL, self.ctrl);
        m.set(KeyMods::ALT, self.alt);
        m.set(KeyMods::SUPER, self.command);
        m
    }
}

/// A non-printable key the frontend forwards by name; printable keys go through
/// [`KmuxDriver::send_char`]. Mirrors the named arm of the GTK frontend's
/// `convert_to_protocol_key`. The daemon turns the resulting [`KeyEvent`] into
/// bytes under the live terminal mode (DECCKM, kitty kbd, modifyOtherKeys).
#[derive(uniffi::Enum)]
pub enum FfiNamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Insert,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
}

impl FfiNamedKey {
    fn to_code(&self) -> KeyCode {
        match self {
            Self::Enter => KeyCode::Enter,
            Self::Tab => KeyCode::Tab,
            Self::Backspace => KeyCode::Backspace,
            Self::Escape => KeyCode::Escape,
            Self::ArrowUp => KeyCode::ArrowUp,
            Self::ArrowDown => KeyCode::ArrowDown,
            Self::ArrowLeft => KeyCode::ArrowLeft,
            Self::ArrowRight => KeyCode::ArrowRight,
            Self::PageUp => KeyCode::PageUp,
            Self::PageDown => KeyCode::PageDown,
            Self::Home => KeyCode::Home,
            Self::End => KeyCode::End,
            Self::Delete => KeyCode::Delete,
            Self::Insert => KeyCode::Insert,
            Self::F1 => KeyCode::F1,
            Self::F2 => KeyCode::F2,
            Self::F3 => KeyCode::F3,
            Self::F4 => KeyCode::F4,
            Self::F5 => KeyCode::F5,
            Self::F6 => KeyCode::F6,
            Self::F7 => KeyCode::F7,
            Self::F8 => KeyCode::F8,
            Self::F9 => KeyCode::F9,
            Self::F10 => KeyCode::F10,
            Self::F11 => KeyCode::F11,
            Self::F12 => KeyCode::F12,
        }
    }
}

/// Mouse button forwarded to a mouse-tracking inner program (left only is wired
/// today; middle/right are encodable for future use).
#[derive(uniffi::Enum)]
pub enum FfiMouseButton {
    Left,
    Middle,
    Right,
}

impl FfiMouseButton {
    fn to_client(&self) -> MouseButton {
        match self {
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
        }
    }
}

/// Whether a pointer event is a button press, release, or motion (drag).
#[derive(uniffi::Enum)]
pub enum FfiMouseKind {
    Press,
    Release,
    Motion,
}

impl FfiMouseKind {
    fn to_client(&self) -> MouseEventKind {
        match self {
            Self::Press => MouseEventKind::Press,
            Self::Release => MouseEventKind::Release,
            Self::Motion => MouseEventKind::Motion,
        }
    }
}

/// Modifiers active during a mouse event. `shift` is the local-selection bypass
/// (never forwarded — see `SessionManager::report_mouse`).
#[derive(uniffi::Record)]
pub struct FfiMouseMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl FfiMouseMods {
    fn to_client(&self) -> MouseMods {
        MouseMods {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
        }
    }
}

/// Cheap grid identity for change detection: the frontend re-fetches
/// [`KmuxDriver::grid_snapshot`] only when a generation differs. `generation`
/// changes on *any* update (cursor move or cell change); `cells_generation`
/// changes only when cells change (so the renderer can skip re-packing cells
/// when only the cursor moved).
#[derive(uniffi::Record)]
pub struct GridInfo {
    pub rows: u32,
    pub cols: u32,
    pub generation: u64,
    pub cells_generation: u64,
}

/// Cursor position + appearance. `shape`: 0=block, 1=underline, 2=bar,
/// 3=hollow-block, 4=hidden.
#[derive(uniffi::Record)]
pub struct FfiCursor {
    pub row: u32,
    pub col: u32,
    pub shape: u8,
    pub visible: bool,
    pub blink: bool,
}

/// The active grid as a packed cell buffer (see [`kmux_render::packed`]) plus
/// dimensions and cursor. `cells` is `rows * cols * 16` bytes, row-major.
#[derive(uniffi::Record)]
pub struct GridSnapshot {
    pub rows: u32,
    pub cols: u32,
    pub cursor: FfiCursor,
    pub cells: Vec<u8>,
}

/// One solid rect of the cursor in physical px — exactly what `kmux_render`
/// would fill (block/bar/underline = 1 rect; hollow-block = 4).
#[derive(uniffi::Record)]
pub struct FfiCursorRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// What the renderer is handed for the focused pane this frame, for the Swift
/// render-debug overlay. Mirrors [`kmux_app::core::RenderDebugSnapshot`] (a flat
/// record, like [`FfiCursor`]), with the cursor's pixel rects computed here via
/// [`kmux_render::cursor_geometry`] from the cell geometry Swift passes in.
///
/// `has_pane` gates the pane fields; `has_cursor` gates the cursor fields (false
/// when no pane is active or it is scrolled into history). `cursor_shape` uses
/// the same code as [`FfiCursor::shape`] (0=block … 4=hidden).
#[derive(uniffi::Record)]
pub struct FfiRenderDebug {
    pub frame_width: u32,
    pub frame_height: u32,
    pub scale: f32,
    pub renderer: String,
    pub blink_on: bool,
    /// The renderer's scale-aware cursor thickness for the passed cell geometry
    /// (compare against the CoreText path's own constants).
    pub cursor_thickness: f32,
    pub has_pane: bool,
    pub pane_id: String,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub scroll_offset: u64,
    pub has_cursor: bool,
    pub cursor_col: u32,
    pub cursor_row: u32,
    pub cursor_shape: u8,
    pub cursor_blink: bool,
    pub cursor_visible: bool,
    pub cursor_is_drawn: bool,
    /// Whether the cursor falls within the grid (else `cursor_rects` is empty).
    pub cursor_in_range: bool,
    pub cursor_cell_x: f32,
    pub cursor_cell_y: f32,
    pub cursor_rects: Vec<FfiCursorRect>,
}

/// An RGB palette color.
#[derive(uniffi::Record)]
pub struct FfiColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<Rgb> for FfiColor {
    fn from(c: Rgb) -> Self {
        Self {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

/// The active toolkit-neutral palette.
#[derive(uniffi::Record)]
pub struct FfiTheme {
    pub bg: FfiColor,
    pub fg: FfiColor,
    pub fg_dim: FfiColor,
    pub accent: FfiColor,
    pub green: FfiColor,
    pub red: FfiColor,
    pub yellow: FfiColor,
    pub purple: FfiColor,
    pub orange: FfiColor,
    pub status_bg: FfiColor,
    pub cursor_bg: FfiColor,
    pub cursor_fg: FfiColor,
}

impl From<&Theme> for FfiTheme {
    fn from(t: &Theme) -> Self {
        Self {
            bg: t.bg.into(),
            fg: t.fg.into(),
            fg_dim: t.fg_dim.into(),
            accent: t.accent.into(),
            green: t.green.into(),
            red: t.red.into(),
            yellow: t.yellow.into(),
            purple: t.purple.into(),
            orange: t.orange.into(),
            status_bg: t.status_bg.into(),
            cursor_bg: t.cursor_bg.into(),
            cursor_fg: t.cursor_fg.into(),
        }
    }
}

/// Whether a [`FfiCellAdjust`] adds pixels or scales by a percentage.
#[derive(uniffi::Enum)]
pub enum FfiCellAdjustKind {
    Pixels,
    Percent,
}

/// A cell-dimension adjustment (mirrors [`CellAdjust`]). `Pixels` adds `value`
/// logical pixels; `Percent` scales by `value`% (e.g. `10.0` → +10%).
#[derive(uniffi::Record)]
pub struct FfiCellAdjust {
    pub kind: FfiCellAdjustKind,
    pub value: f32,
}

impl From<&CellAdjust> for FfiCellAdjust {
    fn from(a: &CellAdjust) -> Self {
        match a {
            CellAdjust::Pixels(v) => Self {
                kind: FfiCellAdjustKind::Pixels,
                value: *v,
            },
            CellAdjust::Percent(v) => Self {
                kind: FfiCellAdjustKind::Percent,
                value: *v,
            },
        }
    }
}

/// The active toolkit-neutral terminal appearance (font + cell geometry). The
/// Swift frontend builds an `NSFont` + CoreText feature settings from this.
#[derive(uniffi::Record)]
pub struct FfiAppearance {
    pub family: String,
    pub family_bold: Option<String>,
    pub family_italic: Option<String>,
    pub family_bold_italic: Option<String>,
    pub size_pt: f32,
    pub style: Option<String>,
    /// OpenType feature settings as harfbuzz tag strings (`"tag=value"`).
    pub features: Vec<String>,
    pub cell_width_adjust: FfiCellAdjust,
    pub cell_height_adjust: FfiCellAdjust,
}

impl From<&Appearance> for FfiAppearance {
    fn from(a: &Appearance) -> Self {
        Self {
            family: a.family.clone(),
            family_bold: a.family_bold.clone(),
            family_italic: a.family_italic.clone(),
            family_bold_italic: a.family_bold_italic.clone(),
            size_pt: a.size_pt,
            style: a.style.clone(),
            features: a
                .features
                .iter()
                .map(kmux_app::appearance::FontFeature::to_setting)
                .collect(),
            cell_width_adjust: (&a.cell_width_adjust).into(),
            cell_height_adjust: (&a.cell_height_adjust).into(),
        }
    }
}

/// Connection lifecycle state (for the connection badge / disconnect overlay).
#[derive(uniffi::Enum)]
pub enum FfiConnStatus {
    Idle,
    Handshaking,
    Connected,
    Reconnecting,
    Disconnected,
}

impl From<&ConnectionState> for FfiConnStatus {
    fn from(s: &ConnectionState) -> Self {
        match s {
            ConnectionState::Idle => Self::Idle,
            ConnectionState::Handshaking => Self::Handshaking,
            ConnectionState::Connected { .. } => Self::Connected,
            ConnectionState::Reconnecting { .. } => Self::Reconnecting,
            ConnectionState::Disconnected { .. } => Self::Disconnected,
        }
    }
}

/// Connection state + a human-readable badge label.
#[derive(uniffi::Record)]
pub struct FfiConnInfo {
    pub status: FfiConnStatus,
    pub label: String,
    /// Whether the transport is pinned via the override (issue #69). When true,
    /// the protocol indicator renders in an "overridden" style.
    pub transport_overridden: bool,
}

/// Recent round-trip-time summary for the active transport (connection
/// inspector). Mirrors `kmux_app::core::RttInfo`.
#[derive(uniffi::Record)]
pub struct FfiRtt {
    /// EWMA latency in ms, or `None` before the first Ping/Pong.
    pub ewma_ms: Option<f64>,
    pub recent_avg_ms: f64,
    pub recent_max_ms: f64,
    pub samples: u64,
}

/// Per-transport byte/message traffic totals (connection inspector). Mirrors
/// `kmux_app::core::TransportTraffic`.
#[derive(uniffi::Record)]
pub struct FfiTransportTraffic {
    pub label: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
}

/// The connection / session / handshake technical details rendered by the
/// connection inspector (issue #60). Mirrors `kmux_app::core::ConnectionInfo`.
#[derive(uniffi::Record)]
pub struct FfiConnectionDetails {
    pub server: String,
    pub is_local: bool,
    pub endpoint: String,
    pub state: String,
    pub connected: bool,
    pub transport: String,
    pub connection_id: Option<u64>,
    pub client_id: Option<u64>,
    pub server_version: Option<String>,
    pub protocol_version: String,
    pub accept_invalid_certs: bool,
    pub rtt: Option<FfiRtt>,
    pub transports: Vec<FfiTransportTraffic>,
}

/// One session in the session list.
#[derive(uniffi::Record)]
pub struct FfiSession {
    pub word_id: String,
    pub name: String,
    pub cwd: String,
    pub active: bool,
    /// The federated peer this session lives on (issue #121), or `None` for a
    /// local session. Lets the sidebar group sessions by machine.
    pub peer: Option<String>,
}

/// One pane (tab) in the active session.
#[derive(uniffi::Record)]
pub struct FfiPane {
    pub id: String,
    /// Display label: the pane title, or `"pane N"` (1-based) when untitled.
    pub label: String,
    pub active: bool,
}

/// Tab label: the pane title, falling back to its 1-based index (mirrors the
/// GTK frontend's `pane_label`).
fn pane_label(index: u32, title: &str) -> String {
    if title.trim().is_empty() {
        format!("pane {}", index + 1)
    } else {
        title.to_string()
    }
}

/// One tab (Session → **Tab** → Pane) of the active session, with the viewed
/// tab flagged. Drives the native tab strip.
#[derive(uniffi::Record)]
pub struct FfiTab {
    pub tab_index: u32,
    pub name: String,
    pub active: bool,
    /// Whether any pane of this tab is currently paused (issue #68); drives the
    /// tab strip's pause marker.
    pub paused: bool,
    /// Whether any pane in this tab has an unread BEL or notification.
    pub needs_attention: bool,
}

/// Tab name, falling back to the focused pane's OSC title, then its 1-based
/// index. An explicit tab rename always wins.
fn tab_label(index: u32, name: &str, pane_title: &str) -> String {
    if name.trim().is_empty() {
        if pane_title.trim().is_empty() {
            format!("{}", index + 1)
        } else {
            pane_title.to_string()
        }
    } else {
        name.to_string()
    }
}

/// What a process-overview row represents (issue #122), driving the Swift
/// view's indent and styling per tier.
#[derive(uniffi::Enum)]
pub enum FfiOverviewKind {
    Session,
    Tab,
    Pane,
    Process,
}

/// One flattened process-overview row (issue #122). Mirrors
/// `kmux_app::core::OverviewRow`; the Swift `ProcessOverviewView` indents by
/// `depth` and right-aligns the CPU/memory/PID columns. Polled via
/// [`KmuxDriver::overview_rows`].
#[derive(uniffi::Record)]
pub struct FfiOverviewRow {
    pub depth: u8,
    pub kind: FfiOverviewKind,
    pub label: String,
    pub detail: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    /// PID for process rows (and the shell pid for pane rows); `None` otherwise.
    pub pid: Option<i32>,
    /// The federated peer this row belongs to (session rows only).
    pub peer: Option<String>,
}

fn overview_kind_to_ffi(kind: OverviewRowKind) -> FfiOverviewKind {
    match kind {
        OverviewRowKind::Session => FfiOverviewKind::Session,
        OverviewRowKind::Tab => FfiOverviewKind::Tab,
        OverviewRowKind::Pane => FfiOverviewKind::Pane,
        OverviewRowKind::Process => FfiOverviewKind::Process,
    }
}

/// One connected client attached to the active session (issue #146). Mirrors
/// `kmux_protocol::messages::ClientInfo`; the Swift `ConnectedClientsView`
/// renders one row per entry with a Kick button. Polled via
/// [`KmuxDriver::client_rows`]; `client_id` is the kick target for
/// [`KmuxDriver::kick_client`].
#[derive(uniffi::Record)]
pub struct FfiClientRow {
    /// Stable per-connection id, passed back to `kick_client`.
    pub client_id: u64,
    /// User-readable label `username@hostname[#N]`.
    pub label: String,
    /// Cryptographic machine identity (hex SHA-256 of the public key).
    pub machine_id: String,
    pub hostname: String,
    pub username: String,
    pub transport: String,
    /// Pane indices of the session this client is viewing.
    pub panes: Vec<u32>,
    /// True for the requester's own connection (rendered as "(you)").
    pub is_self: bool,
}

/// OSC 9;4 (ConEmu/Windows-Terminal) progress-bar state for a pane (issue #125).
/// Mirrors [`PaneProgressState`]; drives the per-pane progress bar the SwiftUI
/// frontend overlays on each tile.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiProgressState {
    /// No bar.
    Remove,
    /// Normal progress (accent).
    Set,
    /// Error (red).
    Error,
    /// Indeterminate / busy (full-width accent).
    Indeterminate,
    /// Paused / warning (amber).
    Pause,
}

impl From<PaneProgressState> for FfiProgressState {
    fn from(s: PaneProgressState) -> Self {
        match s {
            PaneProgressState::Remove => Self::Remove,
            PaneProgressState::Set => Self::Set,
            PaneProgressState::Error => Self::Error,
            PaneProgressState::Indeterminate => Self::Indeterminate,
            PaneProgressState::Pause => Self::Pause,
        }
    }
}

/// One resolved pane rectangle in the active tab, in cell coordinates within the
/// content area passed to [`KmuxDriver::layout`]. `(col, row)` is the top-left
/// corner; the frontend tiles one terminal view per rect and flags the
/// `focused` one. Mirrors `kmux_app::layout::PaneRect` plus the pane id + focus.
#[derive(uniffi::Record)]
pub struct FfiPaneRect {
    pub pane_id: String,
    pub pane_index: u32,
    pub col: u32,
    pub row: u32,
    pub cols: u32,
    pub rows: u32,
    pub focused: bool,
    /// Latest OSC 9;4 progress state for the pane (issue #125); `Remove` = no bar.
    pub progress_state: FfiProgressState,
    /// Progress percentage `0..=100`, or `None` for value-less states.
    pub progress: Option<u8>,
    /// Whether terminal output for this pane is currently withheld by a
    /// connection pause (issue #68) — drives the per-pane "Paused" badge.
    pub paused: bool,
    /// Whether this pane is marked exempt from *auto*-pause (keeps streaming when
    /// the window is backgrounded); drives the pane menu's checkmark (issue #68).
    pub no_auto_pause: bool,
}

/// A per-pane resolved size the frontend pushes down via
/// [`KmuxDriver::set_pane_sizes`] (the analog of the GTK `tiles::push_sizes`):
/// each visible pane's PTY is sized to its tile, not the whole window.
#[derive(uniffi::Record)]
pub struct FfiPaneSize {
    pub pane_id: String,
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// A draggable boundary between two adjacent tiles, mirroring
/// `kmux_app::layout::Divider`. Returned by [`KmuxDriver::dividers`] for
/// hit-testing + cursor; pass one back to [`KmuxDriver::apply_divider_drag`]
/// with a pointer cell to resize. `vertical_bar` is `true` for a vertical bar
/// dragged along the column axis (a col-resize), `false` for a horizontal bar
/// dragged along the row axis (a row-resize).
#[derive(uniffi::Record)]
pub struct FfiDivider {
    pub path: Vec<u32>,
    pub vertical_bar: bool,
    pub before: u32,
    pub hit_col: u32,
    pub hit_row: u32,
    pub hit_cols: u32,
    pub hit_rows: u32,
    pub pair_start: u32,
    pub pair_len: u32,
}

impl FfiDivider {
    fn from_layout(d: kmux_app::layout::Divider) -> Self {
        Self {
            path: d.path,
            vertical_bar: matches!(d.dir, SplitDir::Horizontal),
            before: d.before as u32,
            hit_col: d.hit_col as u32,
            hit_row: d.hit_row as u32,
            hit_cols: d.hit_cols as u32,
            hit_rows: d.hit_rows as u32,
            pair_start: d.pair_start as u32,
            pair_len: d.pair_len as u32,
        }
    }

    fn into_layout(self) -> kmux_app::layout::Divider {
        kmux_app::layout::Divider {
            path: self.path,
            dir: if self.vertical_bar {
                SplitDir::Horizontal
            } else {
                SplitDir::Vertical
            },
            before: self.before as usize,
            hit_col: self.hit_col as u16,
            hit_row: self.hit_row as u16,
            hit_cols: self.hit_cols as u16,
            hit_rows: self.hit_rows as u16,
            pair_start: self.pair_start as u16,
            pair_len: self.pair_len as u16,
        }
    }
}

/// One selected span on a *visible* display row (row 0 = top visible row), in
/// viewport cell coordinates (`col_start..=col_end` inclusive). Returned by
/// [`KmuxDriver::selection`] — one per visible row the selection covers,
/// computed scroll- and wrap-aware by `CellGrid`, so the wash paints over
/// scrollback rows too while scrolled into history.
#[derive(uniffi::Record)]
pub struct FfiSelectionSpan {
    pub row: u32,
    pub col_start: u32,
    pub col_end: u32,
}

/// Scrollback position for the scroll indicator: `offset` lines back from the
/// live bottom, out of `total` scrollback display rows.
#[derive(uniffi::Record)]
pub struct FfiScrollInfo {
    pub offset: u32,
    pub total: u32,
}

/// One autocomplete row for the `/`-command palette. Mirrors `cmd::hint::Hint`;
/// the internal `replace_from` byte offset is omitted (a native text field
/// re-queries [`KmuxDriver::command_hints`] on each change instead of editing
/// char-by-char).
#[derive(uniffi::Record)]
pub struct FfiCommandHint {
    pub display: String,
    pub summary: String,
    pub replacement: String,
    pub append_space: bool,
}

/// Which picker overlay is open.
#[derive(uniffi::Enum)]
pub enum FfiPickerKind {
    Session,
    Directory,
}

/// One row in a picker list.
#[derive(uniffi::Record)]
pub struct FfiPickerEntry {
    pub label: String,
    pub detail: String,
}

/// The open picker's full state, for generic native rendering. Driven via
/// `set_picker_search` / `set_picker_selected` / `activate_picker` /
/// `submit_directory` / `cancel_picker`.
#[derive(uniffi::Record)]
pub struct FfiPicker {
    pub kind: FfiPickerKind,
    pub query: String,
    pub selected: u32,
    pub entries: Vec<FfiPickerEntry>,
}

/// The role of a directory-browser row, so the native UI can render the right
/// glyph and the activation is unambiguous.
#[derive(uniffi::Enum, PartialEq, Eq)]
pub enum FfiDirRowKind {
    /// Create a new session in the browsed directory (row 0).
    CreateHere,
    /// Navigate up to the parent directory.
    Up,
    /// Navigate into a subdirectory.
    Enter,
}

/// One row in the directory browser (the "new session — choose a directory"
/// overlay).
#[derive(uniffi::Record)]
pub struct FfiDirRow {
    pub kind: FfiDirRowKind,
    /// A user-facing label (the directory name, the parent path, or the
    /// "new session in …" affordance).
    pub label: String,
    /// The target path this row acts on (the browsed dir for `CreateHere`, the
    /// parent for Up, the subdir for Enter).
    pub path: String,
}

/// The directory browser's full state, for native rendering. The list lets the
/// user navigate the daemon host's filesystem (so it works for a remote daemon)
/// and pick where a new session is created. Driven via `set_picker_search`
/// (filter), `set_picker_selected`, and `submit_directory` / `activate_picker`
/// (which create-here or navigate based on the selected row); `cancel_picker`
/// dismisses it.
#[derive(uniffi::Record)]
pub struct FfiDirBrowser {
    /// The directory currently being browsed.
    pub cwd: String,
    /// The current filter text.
    pub query: String,
    /// The highlighted row index.
    pub selected: u32,
    /// The browsable rows in render order (`CreateHere`, optional Up, subdirs).
    pub rows: Vec<FfiDirRow>,
    /// A listing error to surface (e.g. permission denied), if any.
    pub error: Option<String>,
}

/// Connection status of a remote in the launcher (issue #121), mirroring
/// [`RemoteStatus`]. The error reason is carried on the row's `detail`.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiRemoteStatus {
    Idle,
    Connecting,
    Connected,
    Error,
}

/// The role of a launcher row, so the native UI renders the right control and
/// activation is unambiguous.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiLaunchRowKind {
    /// Open a new local session (opens the directory browser).
    LocalNewSession,
    /// Attach an existing local session.
    LocalExisting,
    /// A remote's header/toggle row (expand connects on focus).
    Remote,
    /// Open a new session on the remote (opens the path prompt).
    RemoteNewSession,
    /// Attach an existing session on the remote.
    RemoteExisting,
    /// Restore a closed (inactive) local session from the graveyard (issue #64).
    ClosedSession,
    /// Add a new remote (opens the add-remote form).
    AddRemote,
}

/// One row in the unified session launcher (issue #121), flattened for native
/// rendering. `peer`/`word_id` carry the routing keys; `status`/`expanded` drive
/// a remote header; `active` marks the focused session.
#[derive(uniffi::Record)]
pub struct FfiLaunchRow {
    pub kind: FfiLaunchRowKind,
    pub label: String,
    /// Secondary text: a session's cwd, or a remote's status / error reason.
    pub detail: String,
    pub peer: Option<String>,
    pub word_id: Option<String>,
    pub status: FfiRemoteStatus,
    pub expanded: bool,
    pub active: bool,
}

/// The launcher's full state, for native rendering. Driven via the generic
/// `set_picker_search` / `set_picker_selected` / `activate_picker` /
/// `cancel_picker` (the launcher is a picker), plus `launch_*` helpers.
#[derive(uniffi::Record)]
pub struct FfiLaunchPicker {
    pub query: String,
    pub selected: u32,
    pub rows: Vec<FfiLaunchRow>,
}

/// Values for the add-remote form (issue #121), mirroring [`AddRemoteForm`].
#[derive(uniffi::Record)]
pub struct FfiAddRemoteForm {
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub accept_invalid_certs: bool,
}

impl From<FfiAddRemoteForm> for AddRemoteForm {
    fn from(f: FfiAddRemoteForm) -> Self {
        Self {
            host: f.host,
            user: f.user,
            port: f.port,
            accept_invalid_certs: f.accept_invalid_certs,
        }
    }
}

fn remote_status_to_ffi(s: &RemoteStatus) -> FfiRemoteStatus {
    match s {
        RemoteStatus::Idle => FfiRemoteStatus::Idle,
        RemoteStatus::Connecting => FfiRemoteStatus::Connecting,
        RemoteStatus::Connected => FfiRemoteStatus::Connected,
        RemoteStatus::Error(_) => FfiRemoteStatus::Error,
    }
}

/// Flatten a [`LaunchRow`] into its FFI projection.
fn launch_row_to_ffi(row: LaunchRow) -> FfiLaunchRow {
    let idle = FfiLaunchRow {
        kind: FfiLaunchRowKind::AddRemote,
        label: String::new(),
        detail: String::new(),
        peer: None,
        word_id: None,
        status: FfiRemoteStatus::Idle,
        expanded: false,
        active: false,
    };
    match row {
        LaunchRow::LocalNewSession { default_cwd } => FfiLaunchRow {
            kind: FfiLaunchRowKind::LocalNewSession,
            label: "New local session".to_string(),
            detail: default_cwd,
            ..idle
        },
        LaunchRow::LocalExisting {
            word_id,
            name,
            cwd,
            active,
        } => FfiLaunchRow {
            kind: FfiLaunchRowKind::LocalExisting,
            label: name,
            detail: cwd,
            word_id: Some(word_id),
            active,
            ..idle
        },
        LaunchRow::Remote {
            peer,
            label,
            status,
            expanded,
        } => {
            let detail = match &status {
                RemoteStatus::Error(reason) => reason.clone(),
                RemoteStatus::Connecting => "connecting…".to_string(),
                RemoteStatus::Connected => "connected".to_string(),
                RemoteStatus::Idle => String::new(),
            };
            FfiLaunchRow {
                kind: FfiLaunchRowKind::Remote,
                label,
                detail,
                peer: Some(peer),
                status: remote_status_to_ffi(&status),
                expanded,
                ..idle
            }
        }
        LaunchRow::RemoteNewSession { peer } => FfiLaunchRow {
            kind: FfiLaunchRowKind::RemoteNewSession,
            label: "New session…".to_string(),
            peer: Some(peer),
            status: FfiRemoteStatus::Connected,
            ..idle
        },
        LaunchRow::RemoteExisting {
            peer,
            word_id,
            name,
            cwd,
            active,
        } => FfiLaunchRow {
            kind: FfiLaunchRowKind::RemoteExisting,
            label: name,
            detail: cwd,
            peer: Some(peer),
            word_id: Some(word_id),
            status: FfiRemoteStatus::Connected,
            active,
            ..idle
        },
        LaunchRow::ClosedSession {
            word_id,
            name,
            cwd,
            last_active_ms,
        } => {
            let when = kmux_app::core::relative_time_label(last_active_ms);
            let detail = if cwd.is_empty() {
                when
            } else {
                format!("{cwd} · {when}")
            };
            FfiLaunchRow {
                kind: FfiLaunchRowKind::ClosedSession,
                label: name,
                detail,
                word_id: Some(word_id),
                ..idle
            }
        }
        LaunchRow::AddRemote => FfiLaunchRow {
            kind: FfiLaunchRowKind::AddRemote,
            label: "Add remote…".to_string(),
            ..idle
        },
    }
}

/// A user-facing label for a directory-browser row, shared by the generic
/// `picker()` getter and the structured `dir_browser()` getter so both render
/// the row identically.
fn dir_row_label(row: &DirBrowserRow) -> String {
    match row {
        DirBrowserRow::CreateHere { cwd } => format!("＋  New session in {cwd}"),
        DirBrowserRow::Up { parent } => format!("..  {parent}"),
        DirBrowserRow::Enter { name, .. } => format!("📁  {name}"),
    }
}

/// Client-side performance counters for the HUD ticker / metrics inspector.
/// Mirrors `kmux_client::metrics::MetricsSnapshot` + its `DiagCounters`.
#[derive(uniffi::Record)]
pub struct FfiMetrics {
    pub net_apply_avg_ms: f64,
    pub net_apply_max_ms: f64,
    pub apply_avg_ms: f64,
    pub batch_avg: f64,
    pub last_diff_ops: u64,
    pub last_large_diff_ms: f64,
    pub snapshot_mode: bool,
    pub stale_discards: u64,
    pub seqno_gaps: u64,
    pub lag_events: u64,
    pub resyncs: u64,
    // ── Performance counters (issue #61) ──
    /// Whether the latency + FPS counters are enabled (else hidden + uncomputed).
    pub show_perf_counters: bool,
    /// Network round-trip latency (ms) for the active transport; `None` before
    /// the first ping round-trip.
    pub net_latency_ms: Option<f64>,
    /// Whether the link has gone quiet (>3× the ping interval): show the ★ star.
    pub latency_stale: bool,
    /// Rendering frames per second (actual repaints; idles near 0, peaks ~60).
    pub render_fps: u32,
}

/// Which interaction mode / overlay is active. Carries the text the matching
/// overlay needs (connecting label, disconnect reason); list contents are read
/// via the dedicated getters.
#[derive(uniffi::Enum)]
pub enum FfiMode {
    Normal,
    Locked,
    SessionPicker,
    DirectoryPicker,
    /// Unified session launcher (issue #121); rows via `launch_picker()`.
    LaunchPicker,
    /// Add-a-remote form (issue #121); submit via `submit_add_remote`.
    AddRemote,
    /// New-session-on-a-remote path prompt (issue #121); `peer` is the target,
    /// submit via `submit_remote_new_session`.
    RemoteNewSession {
        peer: String,
    },
    Help,
    /// Process overview main-area view (issue #122); rows via `overview_rows()`.
    ProcessOverview,
    /// Connected-clients main-area view (issue #146); rows via `client_rows()`.
    ConnectedClients,
    ConfirmCloseSession {
        word_id: String,
        name: String,
    },
    Command,
    Connecting {
        label: String,
    },
    Disconnected {
        reason: String,
    },
    Other,
}

fn mode_to_ffi(mode: &Mode) -> FfiMode {
    match mode {
        Mode::Normal => FfiMode::Normal,
        Mode::Locked => FfiMode::Locked,
        Mode::SessionPicker => FfiMode::SessionPicker,
        Mode::DirectoryPicker => FfiMode::DirectoryPicker,
        Mode::LaunchPicker => FfiMode::LaunchPicker,
        Mode::AddRemote => FfiMode::AddRemote,
        Mode::RemoteNewSession { peer } => FfiMode::RemoteNewSession { peer: peer.clone() },
        Mode::ProcessOverview => FfiMode::ProcessOverview,
        Mode::ConnectedClients => FfiMode::ConnectedClients,
        Mode::ConfirmCloseSession { word_id, name } => FfiMode::ConfirmCloseSession {
            word_id: word_id.clone(),
            name: name.clone(),
        },
        Mode::Help => FfiMode::Help,
        Mode::Command(_) => FfiMode::Command,
        Mode::Connecting { target_display } => FfiMode::Connecting {
            label: target_display.clone(),
        },
        Mode::Disconnected { reason } => FfiMode::Disconnected {
            reason: reason.clone(),
        },
        _ => FfiMode::Other,
    }
}

/// Build an [`AppCore`] from a [`DriverConfig`], resolving the server target and
/// theme exactly as `kmux_app::launch::run_cli` does for the Rust frontends.
/// Install the client tracing subscriber the first time a driver is built, so
/// the Swift app's logs — this crate, `kmux-render`, and the rest of the client
/// stack — land in the client log file, exactly like the GTK/CLI front door's
/// `run_cli`. Without this the FFI path had no subscriber and dropped every
/// event, which is what made early GPU bugs undiagnosable (PR #144 review).
///
/// Guarded by `Once`: [`kmux_app::launch::init_logging`] sets the *global*
/// default subscriber, which must happen at most once per process (a second
/// `new` would otherwise panic). Honors `RUST_LOG` / `KMUX_LOG_STDERR`.
fn init_ffi_logging(instance_id: &str) {
    static FFI_LOGGING: Once = Once::new();
    FFI_LOGGING.call_once(|| kmux_app::launch::init_logging(instance_id));
}

/// Resolve the startup directory for the native GUI.
///
/// Unlike a CLI process, an app bundle is not launched from a meaningful shell
/// directory (macOS commonly gives it `/`). New GUI sessions therefore start
/// in the user's home directory. Keep a current-directory fallback for unusual
/// environments without `HOME`; explicit launch paths remain `auto_cwd` and
/// take precedence later in [`AppCore::auto_select_session`].
fn gui_initial_cwd() -> String {
    select_gui_initial_cwd(
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        std::env::current_dir().ok().as_deref(),
    )
}

fn select_gui_initial_cwd(home: Option<&Path>, current_dir: Option<&Path>) -> String {
    home.or(current_dir)
        .and_then(Path::to_str)
        .unwrap_or_default()
        .to_string()
}

fn build_core(config: &DriverConfig, instance_id: String) -> AppCore {
    let (target, parsed_server) = parse_target(config.server.as_deref(), config.ssh_port);
    let auto_cwd = config
        .cwd
        .clone()
        .or_else(|| parsed_server.as_ref().and_then(|p| p.path.clone()));
    let theme = config::resolve_theme(config.theme.as_deref());
    // No `--font` flag on the Swift path; the appearance resolves from
    // `config.toml` (mirroring how `theme`/`cursor_blink` default here).
    let appearance = config::resolve_appearance(None);
    let cursor_blink = config::resolve_cursor_blink(config.cursor_blink);
    let initial_cwd = gui_initial_cwd();
    // `kmux diagnostic <test>` on macOS: the Swift app forwards the test name
    // here; resolve it to the same emitter command the GTK path uses (issue
    // #145). An unknown name or a missing `kmux` binary degrades to an ordinary
    // shell launch rather than failing the driver.
    let initial_program = config.diagnostic.as_deref().and_then(|name| {
        let test = DiagnosticTest::from_name(name)?;
        match diagnostic::session_command(test) {
            Ok(cmd) => Some(cmd),
            Err(e) => {
                tracing::warn!(error = %e, test = name, "diagnostic launch unavailable; opening a shell");
                None
            }
        }
    });
    // GUI capabilities: truecolor on, no kitty keyboard/graphics concept.
    let capabilities = ClientCapabilities {
        truecolor: true,
        kitty_graphics: false,
        kitty_keyboard: false,
        term: None,
        term_program: Some("kmux-macos".to_string()),
    };
    let term_size = TermSize {
        rows: config.rows,
        cols: config.cols,
        pixel_width: config.pixel_width,
        pixel_height: config.pixel_height,
    };
    AppCore::new(
        target,
        initial_cwd,
        instance_id,
        config.session.clone(),
        auto_cwd,
        initial_program,
        capabilities,
        theme,
        appearance,
        cursor_blink,
        term_size,
    )
}

/// Opaque, thread-confined handle wrapping a [`FrontendDriver`] and the tokio
/// runtime its background tasks run on. See the module docs for the threading
/// contract.
#[derive(uniffi::Object)]
pub struct KmuxDriver {
    rt: Runtime,
    inner: Mutex<FrontendDriver>,
}

#[uniffi::export]
impl KmuxDriver {
    /// Build a driver and kick off the initial connection (per `config`).
    #[uniffi::constructor]
    pub fn new(config: DriverConfig) -> Result<Arc<Self>, FfiError> {
        // Generate the instance id once and share it between logging and the
        // core, so the client log file and the daemon correlate by the same id.
        let instance_id = generate_instance_id();
        init_ffi_logging(&instance_id);
        // Identify this process as the Swift frontend so every Auth frame reports
        // `frontend = swift` for daemon-side attribution and `kmux clients`.
        kmux_client::set_frontend_kind(kmux_protocol::messages::FrontendKind::Swift);
        tracing::info!(
            server = ?config.server,
            session = ?config.session,
            cols = config.cols,
            rows = config.rows,
            "kmux-ffi: constructing KmuxDriver"
        );
        let rt = Runtime::new().map_err(|e| {
            tracing::error!(error = %e, "kmux-ffi: tokio runtime init failed");
            FfiError::Init {
                message: e.to_string(),
            }
        })?;
        let core = build_core(&config, instance_id);
        // `FrontendDriver::new` spawns the initial bootstrap, so build it with
        // the runtime entered.
        let driver = {
            let _guard = rt.enter();
            FrontendDriver::new(core)
        };
        Ok(Arc::new(Self {
            rt,
            inner: Mutex::new(driver),
        }))
    }

    /// The ABI version this library was built with (see [`KMUX_FFI_ABI_VERSION`]).
    pub fn abi_version(&self) -> u32 {
        KMUX_FFI_ABI_VERSION
    }

    /// Run one pump iteration and return the effects to act on. Call once per
    /// frame. The runtime is entered so the driver's outcome arm can spawn the
    /// SSH supervisor.
    pub fn tick(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        // Reconnect results spawn from the driver, so keep a runtime guard.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.tick().into_iter().map(FfiEffect::from).collect()
    }

    /// Dispatch a curated action; returns any resulting effects. Reconnect /
    /// server-switch are applied internally by the driver.
    pub fn dispatch(&self, action: FfiAction) -> Vec<FfiEffect> {
        let act = Action::from(action);
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(act)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Rebuild the connection channels and reconnect to the current target.
    pub fn reconnect(&self) {
        let _guard = self.rt.enter();
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .reconnect();
    }

    /// Forward raw bytes to the active pane's PTY.
    pub fn send_input(&self, bytes: Vec<u8>) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .send_input(bytes);
    }

    /// Send a printable character as a structured key event. `text` is the
    /// character the keystroke produces (e.g. macOS `charactersIgnoringModifiers`);
    /// `mods` carries the active modifiers. The daemon encodes the bytes under the
    /// live terminal mode, so the frontend never hand-rolls escape sequences.
    /// No-op for empty `text`.
    pub fn send_char(&self, text: String, mods: FfiKeyMods) {
        let Some(ch) = text.chars().next() else {
            return;
        };
        let (code, text, unshifted_codepoint) = char_to_proto_key(ch);
        self.send_key_event(KeyEvent {
            code,
            mods: mods.to_proto(),
            action: KeyAction::Press,
            text,
            unshifted_codepoint,
        });
    }

    /// Send a named key (Enter, arrows, function keys, …) as a structured key
    /// event. See [`send_char`](Self::send_char) for the encoding contract.
    pub fn send_named_key(&self, key: FfiNamedKey, mods: FfiKeyMods) {
        self.send_key_event(KeyEvent {
            code: key.to_code(),
            mods: mods.to_proto(),
            action: KeyAction::Press,
            text: String::new(),
            unshifted_codepoint: 0,
        });
    }

    /// Feed clipboard text back as a paste (in response to
    /// [`FfiEffect::RequestPaste`]).
    pub fn feed_paste(&self, text: String) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .feed_paste(text);
    }

    /// Report a new content size immediately (no debounce).
    pub fn set_term_size(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .set_term_size(TermSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            });
    }

    /// Report a new content size, debounced (applied from a later [`tick`]).
    ///
    /// [`tick`]: KmuxDriver::tick
    pub fn request_resize(&self, rows: u16, cols: u16, pixel_width: u16, pixel_height: u16) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .request_resize(TermSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            });
    }

    /// Report whether the app is backgrounded/inactive, for auto-pause (issue
    /// #68). Backgrounding arms a short debounce before the connection pauses;
    /// foregrounding resumes immediately. Drive this from SwiftUI's `scenePhase`
    /// (and/or `NSWindow.occlusionState`). A manual pause is unaffected.
    pub fn set_window_background(&self, backgrounded: bool) {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .set_window_background(backgrounded);
    }

    /// Current connection pause state for a status indicator (issue #68).
    pub fn pause_state(&self) -> FfiPauseState {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .core()
            .pause_reason()
            .into()
    }

    /// Toggle a pane's exemption from *auto*-pause (issue #68): it keeps
    /// streaming when the window is backgrounded. Drives the pane context-menu
    /// toggle (a manual pause still pauses it).
    pub fn toggle_pane_no_auto_pause(&self, pane_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.toggle_pane_no_auto_pause(&pane_id);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Toggle a whole session's exemption from auto-pause (issue #68); every
    /// pane in the session inherits it. Drives the session context-menu toggle.
    pub fn toggle_session_no_auto_pause(&self, word_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.toggle_session_no_auto_pause(&word_id);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Whether `word_id` is marked exempt from auto-pause at the session level
    /// (session menu checkmark; issue #68).
    pub fn session_no_auto_pause(&self, word_id: String) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .core()
            .session_no_auto_pause(&word_id)
    }

    /// Cheap grid identity for change detection (`None` if no active pane).
    pub fn grid_info(&self) -> Option<GridInfo> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.active_grid().map(|g| GridInfo {
            rows: g.rows as u32,
            cols: g.cols as u32,
            generation: g.generation(),
            cells_generation: g.cells_generation(),
        })
    }

    /// The active grid packed for rendering (`None` if no active pane).
    pub fn grid_snapshot(&self) -> Option<GridSnapshot> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let grid = d.active_grid()?;
        let cells = packed::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: packed::cursor_shape_code(c.shape),
                visible: c.visible,
                blink: c.blink,
            },
            cells,
        })
    }

    /// The active palette (for the renderer + native chrome).
    pub fn theme(&self) -> FfiTheme {
        FfiTheme::from(&self.inner.lock().expect("driver mutex poisoned").palette)
    }

    /// The active terminal appearance (font family/size/style, OpenType
    /// features, cell adjustments) the renderer builds its `NSFont` from.
    pub fn appearance(&self) -> FfiAppearance {
        FfiAppearance::from(&self.inner.lock().expect("driver mutex poisoned").appearance)
    }

    /// The current connection state + badge label.
    pub fn connection(&self) -> FfiConnInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let state = d.mgr.connection_state();
        FfiConnInfo {
            status: FfiConnStatus::from(state),
            label: state.badge_label(),
            transport_overridden: d.mgr.transport_override().is_some(),
        }
    }

    /// Whether a pane is in its soft-close grace window (issue #86), so the
    /// frontend can show an "Undo" affordance.
    pub fn soft_close_pending(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .has_pending_close()
    }

    /// The session list, with the active session flagged.
    pub fn sessions(&self) -> Vec<FfiSession> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_session().map(ToString::to_string);
        d.mgr
            .session_list()
            .iter()
            .map(|e| FfiSession {
                active: active.as_deref() == Some(e.meta.word_id.as_str()),
                word_id: e.meta.word_id.clone(),
                name: e.meta.name.clone(),
                cwd: e.meta.cwd.clone(),
                peer: e.peer.clone(),
            })
            .collect()
    }

    /// The process-overview rows (issue #122): a flat, depth-tagged
    /// Session → Tab → Pane → Process tree joined with the latest CPU/memory
    /// snapshot. Polled by the Swift `ProcessOverviewView` while
    /// [`FfiMode::ProcessOverview`] is active; the driver re-requests the
    /// snapshot at ~1 Hz.
    pub fn overview_rows(&self) -> Vec<FfiOverviewRow> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.overview_rows()
            .into_iter()
            .map(|r| FfiOverviewRow {
                depth: r.depth,
                kind: overview_kind_to_ffi(r.kind),
                label: r.label,
                detail: r.detail,
                cpu_percent: r.cpu_percent,
                mem_bytes: r.mem_bytes,
                pid: r.pid,
                peer: r.peer,
            })
            .collect()
    }

    /// The connected clients of the active session (issue #146). Polled by the
    /// Swift `ConnectedClientsView` while [`FfiMode::ConnectedClients`] is active;
    /// the driver re-requests the list at ~1 Hz.
    pub fn client_rows(&self) -> Vec<FfiClientRow> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.client_rows()
            .into_iter()
            .map(|c| FfiClientRow {
                client_id: c.client_id.0,
                label: c.label,
                machine_id: c.machine_id,
                hostname: c.hostname,
                username: c.username,
                transport: c.transport,
                panes: c.attached_panes,
                is_self: c.is_self,
            })
            .collect()
    }

    /// Kick the client connection `client_id` from the session whose list is
    /// currently shown (issue #146). The list refreshes on the next poll.
    pub fn kick_client(&self, client_id: u64) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.kick_listed_client(kmux_protocol::messages::ClientId(client_id));
    }

    /// The panes (tabs) of the active session, with the active pane flagged.
    pub fn panes(&self) -> Vec<FfiPane> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_pane_id().map(ToString::to_string);
        d.mgr
            .active_session_panes()
            .iter()
            .map(|p| FfiPane {
                active: active.as_deref() == Some(p.pane_id.as_str()),
                id: p.pane_id.clone(),
                label: pane_label(p.pane_index, &p.title),
            })
            .collect()
    }

    /// Focus a pane by id (a tab click). Returns any resulting effects.
    pub fn select_pane(&self, id: String) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::SelectPane(id))
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    // ── Tiling (Session → Tab → Pane) ────────────────────────────────────────

    /// The tabs of the active session, with the viewed tab flagged.
    pub fn tabs(&self) -> Vec<FfiTab> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_tab();
        let word = d
            .mgr
            .active_session()
            .map(ToString::to_string)
            .unwrap_or_default();
        d.mgr
            .active_session_tabs()
            .iter()
            .map(|t| {
                // A tab is paused if any of its panes is paused (issue #68).
                let paused = t
                    .layout
                    .leaves()
                    .iter()
                    .any(|idx| d.core().is_pane_paused(&format_pane_id(&word, *idx)));
                let focused_pane = format_pane_id(&word, t.focused_pane);
                let pane_title = d
                    .mgr
                    .pane_info(&focused_pane)
                    .map(|pane| pane.title.as_str())
                    .unwrap_or_default();
                let needs_attention = t
                    .layout
                    .leaves()
                    .iter()
                    .any(|idx| d.mgr.pane_needs_attention(&format_pane_id(&word, *idx)));
                FfiTab {
                    tab_index: t.tab_index,
                    name: tab_label(t.tab_index, &t.name, pane_title),
                    active: active == Some(t.tab_index),
                    paused,
                    needs_attention,
                }
            })
            .collect()
    }

    /// View a tab of the active session by index (a tab-strip click): attaches
    /// its pane set and focuses its pane. Signals a render.
    pub fn select_tab(&self, tab_index: u32) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.select_tab(tab_index);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Focus a tiled pane by id within the active tab (a click on a tile, or a
    /// keyboard focus move resolved frontend-side). Publishes the shared focus to
    /// the server. Signals a render.
    pub fn focus_pane(&self, pane_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.focus_pane(pane_id);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Resolve the active tab's layout tree into per-pane cell rectangles within
    /// an `area_cols × area_rows` content area, via the shared `kmux_app::layout`
    /// resolver (so every frontend computes identical geometry — the determinism
    /// contract that keeps PTYs from thrashing). Empty when there is no active
    /// tab. The frontend tiles one terminal view per rect, then pushes the
    /// resolved sizes back via [`set_pane_sizes`](Self::set_pane_sizes).
    pub fn layout(&self, area_cols: u16, area_rows: u16) -> Vec<FfiPaneRect> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(word) = d.mgr.active_session().map(ToString::to_string) else {
            return Vec::new();
        };
        let focused = d.mgr.active_pane_id().and_then(pane_index);
        // `render_layout` collapses to the focused pane when zoomed.
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        kmux_app::layout::resolve_layout(
            &layout,
            area_cols,
            area_rows,
            &kmux_app::layout::LayoutConfig::default(),
        )
        .into_iter()
        .map(|r| {
            let pane_id = format_pane_id(&word, r.pane_index);
            let (progress_state, progress) = d
                .mgr
                .pane_info(&pane_id)
                .map_or((FfiProgressState::Remove, None), |p| {
                    (p.progress_state.into(), p.progress)
                });
            let paused = d.core().is_pane_paused(&pane_id);
            let no_auto_pause = d.core().pane_no_auto_pause(&pane_id);
            FfiPaneRect {
                pane_id,
                pane_index: r.pane_index,
                col: r.col as u32,
                row: r.row as u32,
                cols: r.cols as u32,
                rows: r.rows as u32,
                focused: focused == Some(r.pane_index),
                progress_state,
                progress,
                paused,
                no_auto_pause,
            }
        })
        .collect()
    }

    /// Push the resolved per-pane sizes for the visible set; each changed pane's
    /// PTY is resized to its tile. Compute these from [`layout`](Self::layout)
    /// rects × the cell pixel size (mirrors the GTK `tiles::push_sizes`).
    pub fn set_pane_sizes(&self, sizes: Vec<FfiPaneSize>) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let mapped = sizes
            .into_iter()
            .map(|s| {
                (
                    s.pane_id,
                    TermSize {
                        rows: s.rows,
                        cols: s.cols,
                        pixel_width: s.pixel_width,
                        pixel_height: s.pixel_height,
                    },
                )
            })
            .collect();
        d.mgr.set_pane_sizes(mapped);
    }

    /// Enumerate the active tab's draggable dividers within an
    /// `area_cols × area_rows` content area, via the shared
    /// `kmux_app::layout::resolve_dividers` (so divider geometry matches the
    /// tiles from [`layout`](Self::layout)). Empty when there is no active tab
    /// or the focused pane is zoomed (a single tile has no boundary). The
    /// frontend hit-tests a pointer against the `hit_*` strip for the resize
    /// cursor + drag start.
    pub fn dividers(&self, area_cols: u16, area_rows: u16) -> Vec<FfiDivider> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        kmux_app::layout::resolve_dividers(
            &layout,
            area_cols,
            area_rows,
            &kmux_app::layout::LayoutConfig::default(),
        )
        .into_iter()
        .map(FfiDivider::from_layout)
        .collect()
    }

    /// Resize a split by dragging its `divider` so the boundary sits at
    /// `pointer_cell` (cells along the divider's drag axis). Recomputes the new
    /// ratios against the current tree via `kmux_app::layout::ratios_for_drag`
    /// and sends `SetLayoutRatios` (the same wire path as keyboard resize; the
    /// server clamps, renormalizes, and broadcasts). No-op (empty effects) when
    /// the split was reshaped or the move clamps to nothing. Signals a render.
    pub fn apply_divider_drag(&self, divider: FfiDivider, pointer_cell: u32) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        let divider = divider.into_layout();
        let Some(ratios) =
            kmux_app::layout::ratios_for_drag(&layout, &divider, pointer_cell as u16)
        else {
            return Vec::new();
        };
        let core = d.core_mut();
        core.mgr.set_layout_ratios(divider.path, ratios);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Reset the split a `divider` belongs to back to even children (a
    /// double-click on the divider). No-op when the divider's split is gone.
    /// Signals a render.
    pub fn reset_divider(&self, divider: FfiDivider) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(layout) = d.mgr.render_layout() else {
            return Vec::new();
        };
        let path = divider.path;
        let Some(ratios) = kmux_app::layout::even_ratios_at(&layout, &path) else {
            return Vec::new();
        };
        let core = d.core_mut();
        core.mgr.set_layout_ratios(path, ratios);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Rename a tab of the active session (a native rename sheet). Signals a
    /// render.
    pub fn rename_tab(&self, tab_index: u32, name: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.rename_tab(tab_index, &name);
        core.request_render();
        vec![FfiEffect::NeedsRender]
    }

    /// Move a tab to a zero-based position in the active session.
    pub fn reorder_tab(&self, tab_index: u32, new_position: u32) {
        self.inner
            .lock()
            .unwrap()
            .mgr
            .reorder_tab(tab_index, new_position);
    }

    /// Cheap grid identity for a specific pane (per-tile change detection).
    pub fn grid_info_for(&self, pane_id: String) -> Option<GridInfo> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        d.mgr.buffer(&pane_id).map(|g| GridInfo {
            rows: g.rows as u32,
            cols: g.cols as u32,
            generation: g.generation(),
            cells_generation: g.cells_generation(),
        })
    }

    /// A specific pane's grid packed for rendering (`None` if not attached).
    pub fn grid_snapshot_for(&self, pane_id: String) -> Option<GridSnapshot> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let grid = d.mgr.buffer(&pane_id)?;
        let cells = packed::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: packed::cursor_shape_code(c.shape),
                visible: c.visible,
                blink: c.blink,
            },
            cells,
        })
    }

    /// A specific pane's selection spans (for its selection wash).
    pub fn selection_for(&self, pane_id: String) -> Vec<FfiSelectionSpan> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(grid) = d.mgr.buffer(&pane_id) else {
            return Vec::new();
        };
        grid.visible_selection_spans()
            .into_iter()
            .map(|(row, col_start, col_end)| FfiSelectionSpan {
                row: row as u32,
                col_start: col_start as u32,
                col_end: col_end as u32,
            })
            .collect()
    }

    /// A specific pane's scrollback position (for its scroll indicator).
    pub fn scroll_info_for(&self, pane_id: String) -> FfiScrollInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        match d.mgr.buffer(&pane_id) {
            Some(g) => FfiScrollInfo {
                offset: g.scroll_offset() as u32,
                total: g.total_scrollback_display_rows() as u32,
            },
            None => FfiScrollInfo {
                offset: 0,
                total: 0,
            },
        }
    }

    /// Set a normal (drag) selection between two *visible* viewport cells. The
    /// cells are mapped scroll-aware, so this works while scrolled into history.
    pub fn set_selection(&self, anchor_row: u32, anchor_col: u32, end_row: u32, end_col: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr.active_grid_mut() {
            let anchor = grid.visible_to_abs(anchor_row as usize, anchor_col as usize);
            let end = grid.visible_to_abs(end_row as usize, end_col as usize);
            grid.set_selection(Some(Selection {
                anchor,
                end,
                mode: SelectionMode::Normal,
            }));
        }
    }

    /// Select the word at a *visible* viewport cell (double-click).
    pub fn select_word_at(&self, row: u32, col: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr.active_grid_mut() {
            let pos = grid.visible_to_abs(row as usize, col as usize);
            let (anchor, end) = grid.find_word_boundaries(pos);
            grid.set_selection(Some(Selection {
                anchor,
                end,
                mode: SelectionMode::Word,
            }));
        }
    }

    /// Select the whole line at a *visible* viewport row (triple-click).
    pub fn select_line_at(&self, row: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr.active_grid_mut() {
            let cols = grid.cols;
            let abs_row = grid.visible_to_abs(row as usize, 0).row;
            grid.set_selection(Some(Selection {
                anchor: GridPos {
                    row: abs_row,
                    col: 0,
                },
                end: GridPos {
                    row: abs_row,
                    col: cols.saturating_sub(1),
                },
                mode: SelectionMode::Line,
            }));
        }
    }

    /// Clear the active selection.
    pub fn clear_selection(&self) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr.active_grid_mut() {
            grid.clear_selection();
        }
    }

    /// The active selection as per-visible-row spans (for the selection wash),
    /// empty when there is no selection. Scroll- and wrap-aware, so the wash
    /// paints over scrollback rows while scrolled into history.
    pub fn selection(&self) -> Vec<FfiSelectionSpan> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let Some(grid) = d.mgr.active_grid() else {
            return Vec::new();
        };
        grid.visible_selection_spans()
            .into_iter()
            .map(|(row, col_start, col_end)| FfiSelectionSpan {
                row: row as u32,
                col_start: col_start as u32,
                col_end: col_end as u32,
            })
            .collect()
    }

    /// Mouse-wheel scroll at a *visible* viewport cell. Forwards an SGR/X10 wheel
    /// event to the PTY when the pane has mouse reporting on; otherwise scrolls
    /// local scrollback. `lines` > 0 scrolls up (into history). Mirrors the GTK
    /// frontend's `scroll_pane`.
    pub fn scroll_at(&self, col: u32, row: u32, lines: i32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let Some(pane_id) = d.mgr.active_pane_id().map(ToString::to_string) else {
            return;
        };
        let use_pty = d
            .mgr
            .buffer(&pane_id)
            .is_some_and(|g| g.modes().mouse_report());
        if use_pty {
            let sgr = d
                .mgr
                .buffer(&pane_id)
                .is_some_and(|g| g.modes().sgr_mouse());
            let bytes = encode_mouse_scroll(col as u16 + 1, row as u16 + 1, lines, sgr);
            if !bytes.is_empty() {
                d.send_input(bytes);
            }
        } else if let Some(grid) = d.mgr.buffer_mut(&pane_id) {
            if lines > 0 {
                grid.scroll_up(lines as usize);
            } else {
                grid.scroll_down((-lines) as usize);
            }
        }
    }

    /// Forward a mouse button/drag/release to the active pane's inner program
    /// when it has enabled mouse tracking, returning `true` iff it was sent (the
    /// frontend then skips its own client-side text selection). `col`/`row` are
    /// 0-based *visible* viewport cells (converted to the 1-based terminal
    /// coordinates here, like [`KmuxDriver::scroll_at`]). `button_held` gates
    /// motion under button-event tracking (mode 1002). A shift-held event is
    /// never forwarded — Shift is the local-selection bypass. Mirrors the GTK
    /// frontend's `report_mouse` calls; the policy lives in
    /// `SessionManager::report_mouse`.
    pub fn mouse_event(
        &self,
        col: u32,
        row: u32,
        button: FfiMouseButton,
        kind: FfiMouseKind,
        mods: FfiMouseMods,
        button_held: bool,
    ) -> bool {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let ev = MouseEvent {
            button: button.to_client(),
            kind: kind.to_client(),
            col: col as u16 + 1,
            row: row as u16 + 1,
            mods: mods.to_client(),
        };
        d.mgr.report_mouse(button_held, ev)
    }

    /// Scroll the active pane's *local* scrollback by `lines` display rows
    /// (`> 0` = up into history). Unlike [`KmuxDriver::scroll_at`] this never
    /// forwards to the PTY — used for drag auto-scroll, which must reveal
    /// scrollback for selection regardless of PTY mouse reporting (mirrors the
    /// GTK frontend's direct `grid.scroll_up/scroll_down`).
    pub fn scroll_lines(&self, lines: i32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(grid) = d.mgr.active_grid_mut() {
            if lines > 0 {
                grid.scroll_up(lines as usize);
            } else {
                grid.scroll_down((-lines) as usize);
            }
        }
    }

    /// Scrollback position for the scroll indicator.
    pub fn scroll_info(&self) -> FfiScrollInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        match d.mgr.active_grid() {
            Some(g) => FfiScrollInfo {
                offset: g.scroll_offset() as u32,
                total: g.total_scrollback_display_rows() as u32,
            },
            None => FfiScrollInfo {
                offset: 0,
                total: 0,
            },
        }
    }

    /// Autocomplete hints for an arbitrary `/`-command-palette input, without
    /// changing the current mode. For a native palette that owns its own text
    /// field instead of driving `Mode::Command` char-by-char.
    pub fn command_hints(&self, input: String) -> Vec<FfiCommandHint> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        // Hints are computed from the buffer in `Mode::Command`; flip into it
        // transiently (atomic under the lock) and restore so this is a pure query.
        let prev = std::mem::replace(
            &mut core.mode,
            Mode::Command(CommandState {
                buffer: input.clone(),
                cursor: input.len(),
                ..CommandState::default()
            }),
        );
        let hints = cmd::hint::build_hints(core);
        core.mode = prev;
        hints
            .into_iter()
            .map(|h| FfiCommandHint {
                display: h.display,
                summary: h.summary.to_string(),
                replacement: h.replacement,
                append_space: h.append_space,
            })
            .collect()
    }

    /// Parse and execute a `/`-command line in one shot (reconnect / server
    /// switch applied internally), returning any resulting effects.
    pub fn run_command(&self, input: String) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.core_mut().mode = Mode::Command(CommandState {
            buffer: input.clone(),
            cursor: input.len(),
            ..CommandState::default()
        });
        d.dispatch_action(Action::CommandSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// The currently open picker (session / server / directory), or `None`.
    pub fn picker(&self) -> Option<FfiPicker> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        match core.mode {
            Mode::SessionPicker => {
                let mut entries = vec![FfiPickerEntry {
                    label: "[+] New session".to_string(),
                    detail: String::new(),
                }];
                for e in core.session_picker_matches() {
                    entries.push(FfiPickerEntry {
                        label: core.mgr.display_name_for(&e.meta.word_id),
                        detail: e.meta.cwd.clone(),
                    });
                }
                Some(FfiPicker {
                    kind: FfiPickerKind::Session,
                    query: core.session_picker_search.clone(),
                    selected: core.session_picker_selected as u32,
                    entries,
                })
            }
            Mode::DirectoryPicker => {
                // The directory picker is a *browser* of the daemon host's
                // filesystem. The richer per-row state is exposed via
                // `dir_browser()`; this generic getter keeps the picker sheet
                // presenting and shows readable row labels.
                let entries = core
                    .dir_browser_rows()
                    .into_iter()
                    .map(|row| FfiPickerEntry {
                        label: dir_row_label(&row),
                        detail: String::new(),
                    })
                    .collect();
                Some(FfiPicker {
                    kind: FfiPickerKind::Directory,
                    query: core.dir_picker_buffer.clone(),
                    selected: core.dir_picker_selected as u32,
                    entries,
                })
            }
            _ => None,
        }
    }

    /// The directory browser's full state (rows with their kind, the browsed
    /// directory, filter, selection, and any listing error), or `None` when the
    /// directory browser is not open. Backs the native directory-browser UI.
    pub fn dir_browser(&self) -> Option<FfiDirBrowser> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        if !matches!(core.mode, Mode::DirectoryPicker) {
            return None;
        }
        let rows = core
            .dir_browser_rows()
            .into_iter()
            .map(|row| {
                let label = dir_row_label(&row);
                let (kind, path) = match row {
                    DirBrowserRow::CreateHere { cwd } => (FfiDirRowKind::CreateHere, cwd),
                    DirBrowserRow::Up { parent } => (FfiDirRowKind::Up, parent),
                    DirBrowserRow::Enter { path, .. } => (FfiDirRowKind::Enter, path),
                };
                FfiDirRow { kind, label, path }
            })
            .collect();
        Some(FfiDirBrowser {
            cwd: core.dir_browser_cwd.clone(),
            query: core.dir_picker_buffer.clone(),
            selected: core.dir_picker_selected as u32,
            rows,
            error: core.dir_browser_error().map(ToString::to_string),
        })
    }

    /// The unified session launcher's full state (issue #121), or `None` when it
    /// is not open. Driven by the generic picker methods (`set_picker_search`,
    /// `set_picker_selected`, `activate_picker`, `cancel_picker`) plus the
    /// `submit_*` helpers below.
    pub fn launch_picker(&self) -> Option<FfiLaunchPicker> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        if !matches!(core.mode, Mode::LaunchPicker) {
            return None;
        }
        let rows = core
            .launch_rows()
            .into_iter()
            .map(launch_row_to_ffi)
            .collect();
        Some(FfiLaunchPicker {
            query: core.launch_search.clone(),
            selected: core.launch_selected as u32,
            rows,
        })
    }

    /// Open the unified session launcher (the new-session button).
    pub fn open_launch_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::OpenLaunchPicker)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Build a peer from the add-remote form, register + connect it, and persist
    /// SSH ones (issue #121). Returns an error message when the form is
    /// incomplete (and leaves the form open), or `None` on success.
    pub fn submit_add_remote(&self, form: FfiAddRemoteForm) -> Option<String> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        let result = core.submit_add_remote(form.into());
        core.request_render();
        result.err()
    }

    /// Create a new session on a federated `peer` at `cwd` (issue #121). An empty
    /// `cwd` lets the remote daemon resolve a default. Closes the prompt.
    pub fn submit_remote_new_session(&self, peer: String, cwd: String) {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.submit_remote_new_session(peer, cwd);
        core.request_render();
    }

    /// Disconnect a federated remote (issue #121): drop its link and forget it.
    pub fn disconnect_remote(&self, peer: String) {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.disconnect_remote(&peer);
        core.request_render();
    }

    /// Open the session picker.
    pub fn open_session_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::OpenSessionPicker)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Set the open picker's search/filter text (resets the selection to row 0).
    pub fn set_picker_search(&self, text: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.set_picker_search(text);
        core.request_render();
    }

    /// Set the open picker's highlighted row (hover/click).
    pub fn set_picker_selected(&self, index: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.set_picker_selected(index as usize);
        core.request_render();
    }

    /// Activate the open picker's current selection (click / Enter). May switch
    /// servers (server picker) or select a session.
    pub fn activate_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.activate_picker_selection()
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Submit the directory browser's selected row: create-here returns to
    /// normal interaction, navigation (Up / into a subdir, or a typed absolute
    /// path) refreshes the listing in place. Also honors a typed absolute path
    /// in the filter when it matches no listed row.
    pub fn submit_directory(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Activate directory-browser row `index` (a tap): selects it, then submits.
    /// `CreateHere` creates the session and dismisses; Up / a subdirectory
    /// navigate and keep the browser open (it refreshes when the listing lands).
    pub fn dir_browser_activate(&self, index: u32) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.core_mut().set_picker_selected(index as usize);
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Create a new session in the directory currently being browsed (the
    /// `CreateHere` affordance), regardless of the highlighted row.
    pub fn dir_browser_open_here(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.core_mut().set_picker_selected(0);
        d.dispatch_action(Action::DirPickerSubmit)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Close any open picker / overlay (back to normal interaction).
    pub fn cancel_picker(&self) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mode = Mode::Normal;
        core.request_render();
    }

    /// Rename a session by word id (trims surrounding whitespace).
    pub fn rename_session(&self, word_id: String, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.rename_session(&word_id, name.trim());
        core.request_render();
    }

    /// Request confirmation before closing a session by word id.
    pub fn close_session(&self, word_id: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.confirm_close_session(&word_id);
        core.request_render();
    }

    /// Confirm the pending session close, if a close confirmation is open.
    pub fn confirm_close_session(&self) -> Vec<FfiEffect> {
        // A dispatch can spawn (Reconnect rebuilds the bootstrap task;
        // RecentServers::save uses spawn_blocking), so hold a runtime guard
        // even though the dispatch itself is synchronous.
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.dispatch_action(Action::ConfirmCloseSession)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Whether the performance HUD ticker is shown.
    pub fn hud_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .hud_visible
    }

    /// Whether the metrics inspector overlay is open.
    pub fn metrics_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .metrics_overlay_visible
    }

    /// Whether the connection inspector overlay is open (issue #60).
    pub fn connection_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .connection_overlay_visible
    }

    /// Whether the render-debug overlay is shown.
    pub fn render_debug_visible(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .render_debug_visible()
    }

    /// The live connection / session / handshake details for the connection
    /// inspector. Built from the toolkit-neutral `ConnectionInfo`.
    pub fn connection_details(&self) -> FfiConnectionDetails {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let info = d.core().connection_info();
        FfiConnectionDetails {
            server: info.server,
            is_local: info.is_local,
            endpoint: info.endpoint,
            state: info.state,
            connected: info.connected,
            transport: info.transport,
            connection_id: info.connection_id,
            client_id: info.client_id,
            server_version: info.server_version,
            protocol_version: info.protocol_version,
            accept_invalid_certs: info.accept_invalid_certs,
            rtt: info.rtt.map(|r| FfiRtt {
                ewma_ms: r.ewma_ms,
                recent_avg_ms: r.recent_avg_ms,
                recent_max_ms: r.recent_max_ms,
                samples: r.samples,
            }),
            transports: info
                .transports
                .into_iter()
                .map(|t| FfiTransportTraffic {
                    label: t.label,
                    bytes_in: t.bytes_in,
                    bytes_out: t.bytes_out,
                    msgs_in: t.msgs_in,
                    msgs_out: t.msgs_out,
                })
                .collect(),
        }
    }

    /// A snapshot of the client-side performance metrics.
    pub fn metrics(&self) -> FfiMetrics {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core();
        let snap = core.mgr.metrics.snapshot(core.force_snapshot_mode);
        let c = &snap.counters;
        FfiMetrics {
            net_apply_avg_ms: snap.net_apply_avg_ms,
            net_apply_max_ms: snap.net_apply_max_ms,
            apply_avg_ms: snap.apply_avg_ms,
            batch_avg: snap.batch_avg,
            last_diff_ops: snap.last_diff_ops as u64,
            last_large_diff_ms: snap.last_large_diff_ms,
            snapshot_mode: snap.snapshot_mode,
            stale_discards: c.stale_discards,
            seqno_gaps: c.seqno_gaps,
            lag_events: c.lag_events,
            resyncs: c.resyncs,
            show_perf_counters: core.show_perf_counters,
            net_latency_ms: core.net_latency_ms(),
            latency_stale: core.net_latency_stale(),
            render_fps: core.render_fps(),
        }
    }

    /// What the renderer is handed for the focused pane this frame, for the
    /// render-debug overlay. Swift passes its content-area pixel size, scale,
    /// renderer leaf, and cell geometry; the cursor's pixel rects are computed
    /// here via [`kmux_render::cursor_geometry`] so they match the renderer.
    pub fn render_debug(
        &self,
        frame_width: u32,
        frame_height: u32,
        scale: f32,
        renderer: String,
        cell_w: f32,
        cell_h: f32,
    ) -> FfiRenderDebug {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let snap = d.render_debug_snapshot(frame_width, frame_height, scale, &renderer);
        let cell = kmux_render::CellMetrics::new(cell_w, cell_h);

        let mut out = FfiRenderDebug {
            frame_width: snap.frame_width,
            frame_height: snap.frame_height,
            scale: snap.scale,
            renderer: snap.renderer,
            blink_on: snap.blink_on,
            cursor_thickness: cell.cursor_thickness,
            has_pane: false,
            pane_id: String::new(),
            grid_cols: 0,
            grid_rows: 0,
            scroll_offset: 0,
            has_cursor: false,
            cursor_col: 0,
            cursor_row: 0,
            cursor_shape: 0,
            cursor_blink: false,
            cursor_visible: false,
            cursor_is_drawn: false,
            cursor_in_range: false,
            cursor_cell_x: 0.0,
            cursor_cell_y: 0.0,
            cursor_rects: Vec::new(),
        };

        if let Some(p) = snap.pane {
            out.has_pane = true;
            out.pane_id = p.pane_id;
            out.grid_cols = p.grid_cols as u32;
            out.grid_rows = p.grid_rows as u32;
            out.scroll_offset = p.scroll_offset as u64;
            if let Some(c) = p.cursor {
                let cv = CursorView {
                    col: c.col,
                    row: c.row,
                    shape: c.shape,
                    blink: c.blink,
                    visible: c.visible,
                };
                let geo =
                    kmux_render::cursor_geometry(&cv, (0.0, 0.0), p.grid_cols, p.grid_rows, &cell);
                out.has_cursor = true;
                out.cursor_col = c.col as u32;
                out.cursor_row = c.row as u32;
                out.cursor_shape = packed::cursor_shape_code(c.shape);
                out.cursor_blink = c.blink;
                out.cursor_visible = c.visible;
                out.cursor_is_drawn = c.is_drawn;
                out.cursor_in_range = geo.in_range;
                out.cursor_cell_x = geo.cell_origin.0;
                out.cursor_cell_y = geo.cell_origin.1;
                out.cursor_rects = geo
                    .rects
                    .into_iter()
                    .map(|r| FfiCursorRect {
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                    })
                    .collect();
            }
        }
        out
    }

    /// The built-in theme names (for a Preferences theme picker).
    pub fn available_themes(&self) -> Vec<String> {
        theme::BUILTIN_THEMES
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    /// Switch the active palette to a built-in theme by name (no-op if unknown).
    /// The driver emits `PaletteChanged` from the next [`tick`](Self::tick).
    pub fn set_theme(&self, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(t) = theme::builtin_theme(&name) {
            let core = d.core_mut();
            core.palette = t;
            core.request_render();
        }
    }

    /// Whether the cursor is shown on the current frame (blink phase).
    pub fn blink_on(&self) -> bool {
        self.inner.lock().expect("driver mutex poisoned").blink_on()
    }

    /// Whether the inner-pane cursor is allowed to blink (Preferences toggle).
    pub fn cursor_blink_enabled(&self) -> bool {
        self.inner
            .lock()
            .expect("driver mutex poisoned")
            .cursor_blink_enabled
    }

    /// Enable/disable cursor blinking live and persist it to `config.toml`. When
    /// disabled the cursor is drawn steady; the driver pins the blink phase solid
    /// on the next [`tick`](Self::tick).
    pub fn set_cursor_blink_enabled(&self, enabled: bool) {
        {
            let mut d = self.inner.lock().expect("driver mutex poisoned");
            let core = d.core_mut();
            if core.cursor_blink_enabled == enabled {
                return;
            }
            core.cursor_blink_enabled = enabled;
            core.request_render();
        }
        // Persist (load-modify-save so theme/font are preserved), mirroring the
        // GTK preferences window.
        let mut cfg = config::load();
        cfg.cursor_blink = Some(enabled);
        if let Err(e) = config::save(&cfg) {
            tracing::error!("failed to persist cursor_blink: {e}");
        }
    }

    /// Which interaction mode / overlay is active.
    pub fn mode(&self) -> FfiMode {
        mode_to_ffi(&self.inner.lock().expect("driver mutex poisoned").mode)
    }
}

impl KmuxDriver {
    /// Forward one structured key event and reset the blink phase, snapping the
    /// viewport to the live bottom first (mirrors the GTK key handler). Not
    /// exported: `send_char` / `send_named_key` are the public entry points.
    fn send_key_event(&self, ev: KeyEvent) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.scroll_to_bottom();
        d.send_keys(vec![ev]);
    }
}

/// GPU terminal renderer presenting to a macOS `CAMetalLayer` (issue #132).
///
/// An opaque, thread-confined wrapper over [`kmux_render::TerminalRenderer`]
/// (all calls on the Swift main thread). It reads the active tab's grids +
/// layout from a [`KmuxDriver`] and presents directly to the layer — no
/// readback. Built only with the `gpu` feature; the default staticlib omits it.
#[cfg(feature = "gpu")]
#[derive(uniffi::Object)]
pub struct KmuxRenderer {
    inner: Mutex<TerminalRenderer>,
}

#[cfg(feature = "gpu")]
#[uniffi::export]
impl KmuxRenderer {
    /// Build a renderer bound to a `CAMetalLayer` pointer, using the driver's
    /// current appearance + palette. `width`/`height` are physical px.
    ///
    /// The Swift view owns the layer and must keep it alive for the renderer's
    /// lifetime (it drops the renderer before tearing the view down).
    #[uniffi::constructor]
    pub fn new_metal(
        driver: &KmuxDriver,
        layer_ptr: u64,
        width: u32,
        height: u32,
        scale: f32,
    ) -> Result<Arc<Self>, FfiError> {
        tracing::debug!(
            layer_ptr,
            width,
            height,
            scale,
            "KmuxRenderer::new_metal: creating Metal renderer"
        );
        let d = driver.inner.lock().expect("driver mutex poisoned");
        // SAFETY: the Swift view guarantees the layer outlives this renderer.
        let renderer = unsafe {
            TerminalRenderer::new_for_metal_layer(
                layer_ptr,
                width,
                height,
                scale,
                &d.appearance,
                &d.palette,
            )
        }
        .map_err(|e| {
            tracing::error!(width, height, scale, error = %e, "KmuxRenderer::new_metal: GPU init failed");
            FfiError::Render {
                message: e.to_string(),
            }
        })?;
        drop(d);
        Ok(Arc::new(Self {
            inner: Mutex::new(renderer),
        }))
    }

    /// Resize the swapchain to `width × height` physical px at `scale`.
    pub fn resize(&self, width: u32, height: u32, scale: f32) {
        tracing::debug!(width, height, scale, "KmuxRenderer::resize");
        self.inner
            .lock()
            .expect("renderer mutex poisoned")
            .resize(width, height, scale);
    }

    /// Re-read the font appearance from the driver (after a font change).
    pub fn refresh_appearance(&self, driver: &KmuxDriver) {
        tracing::debug!("KmuxRenderer::refresh_appearance");
        let appearance = driver
            .inner
            .lock()
            .expect("driver mutex poisoned")
            .appearance
            .clone();
        self.inner
            .lock()
            .expect("renderer mutex poisoned")
            .set_appearance(&appearance);
    }

    /// Render the active tab and present. `width`/`height` are physical px.
    pub fn render(&self, driver: &KmuxDriver, width: u32, height: u32, scale: f32) {
        tracing::trace!(width, height, scale, "KmuxRenderer::render");
        let mut renderer = self.inner.lock().expect("renderer mutex poisoned");
        let d = driver.inner.lock().expect("driver mutex poisoned");
        render_active_tab(&mut renderer, &d, width, height, scale);
    }

    /// The linked kmux-render API version.
    pub fn api_version(&self) -> u32 {
        kmux_render::KMUX_RENDER_API_VERSION
    }
}

/// Assemble the active tab's frame from the driver and render it. Mirrors the
/// GTK `render_gpu::paint` frame assembly — both read the shared layout + grids
/// and build an identical [`Frame`] with `CellSource::Grid`.
#[cfg(feature = "gpu")]
fn render_active_tab(
    renderer: &mut TerminalRenderer,
    d: &FrontendDriver,
    width: u32,
    height: u32,
    scale: f32,
) {
    if width == 0 || height == 0 {
        return;
    }
    type Entry<'a> = (u16, u16, u16, u16, bool, &'a kmux_client::grid::CellGrid);
    let mut entries: Vec<Entry<'_>> = Vec::new();
    let mut multi = false;
    if let Some(layout) = d.mgr.render_layout() {
        let (cols, rows) = renderer.cols_rows(width as i32, height as i32);
        let rects = kmux_app::layout::resolve_layout(
            &layout,
            cols,
            rows,
            &kmux_app::layout::LayoutConfig::default(),
        );
        multi = rects.len() > 1;
        let focused = d.mgr.active_pane_id().and_then(pane_index);
        let word = d.mgr.active_session().unwrap_or("").to_string();
        for r in &rects {
            let pane_id = format_pane_id(&word, r.pane_index);
            if let Some(grid) = d.mgr.buffer(&pane_id) {
                entries.push((
                    r.col,
                    r.row,
                    r.cols,
                    r.rows,
                    Some(r.pane_index) == focused,
                    grid,
                ));
            }
        }
    } else if let Some(grid) = d.active_grid() {
        entries.push((0, 0, grid.cols as u16, grid.rows as u16, true, grid));
    }

    let spans: Vec<Vec<(u16, u16, u16)>> = entries
        .iter()
        .map(|e| e.5.visible_selection_spans())
        .collect();
    let blink_on = d.blink_on();
    let panes: Vec<PaneView<'_>> = entries
        .iter()
        .zip(spans.iter())
        .map(|(e, sel)| {
            let scrolled = e.5.scroll_offset() > 0;
            PaneView {
                col: e.0,
                row: e.1,
                cols: e.2,
                rows: e.3,
                focused: e.4,
                cells: CellSource::Grid(e.5),
                cursor: (!scrolled).then(|| CursorView::from_state(e.5.cursor())),
                selection: sel,
                scroll: scrolled.then(|| ScrollIndicator {
                    offset: e.5.scroll_offset(),
                    total: e.5.total_scrollback_display_rows(),
                }),
            }
        })
        .collect();

    tracing::trace!(
        panes = panes.len(),
        multi,
        blink_on,
        "kmux-ffi: assembled GPU frame"
    );
    let frame = Frame {
        width,
        height,
        scale,
        palette: &d.palette,
        blink_on,
        panes,
        multi,
    };
    if let Err(e) = renderer.render(&frame) {
        tracing::warn!("kmux-render: metal frame failed: {e}");
    }
}

#[cfg(all(test, feature = "gpu"))]
mod gpu_tests {
    #[test]
    fn render_api_matches_expected() {
        assert_eq!(
            kmux_render::KMUX_RENDER_API_VERSION,
            super::EXPECTED_RENDER_API
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_initial_cwd_prefers_home_over_process_directory() {
        assert_eq!(
            select_gui_initial_cwd(Some(Path::new("/Users/alice")), Some(Path::new("/"))),
            "/Users/alice"
        );
    }

    #[test]
    fn gui_initial_cwd_falls_back_to_process_directory_without_home() {
        assert_eq!(
            select_gui_initial_cwd(None, Some(Path::new("/work/project"))),
            "/work/project"
        );
    }

    #[test]
    fn gui_initial_cwd_is_empty_without_a_resolvable_directory() {
        assert_eq!(select_gui_initial_cwd(None, None), "");
    }

    #[test]
    fn named_key_codes_match_protocol() {
        assert_eq!(FfiNamedKey::Enter.to_code(), KeyCode::Enter);
        assert_eq!(FfiNamedKey::Tab.to_code(), KeyCode::Tab);
        assert_eq!(FfiNamedKey::Backspace.to_code(), KeyCode::Backspace);
        assert_eq!(FfiNamedKey::Escape.to_code(), KeyCode::Escape);
        assert_eq!(FfiNamedKey::ArrowUp.to_code(), KeyCode::ArrowUp);
        assert_eq!(FfiNamedKey::ArrowRight.to_code(), KeyCode::ArrowRight);
        assert_eq!(FfiNamedKey::PageDown.to_code(), KeyCode::PageDown);
        assert_eq!(FfiNamedKey::F5.to_code(), KeyCode::F5);
        assert_eq!(FfiNamedKey::F12.to_code(), KeyCode::F12);
    }

    #[test]
    fn key_mods_map_to_proto_bits() {
        let none = FfiKeyMods {
            shift: false,
            ctrl: false,
            alt: false,
            command: false,
        };
        assert_eq!(none.to_proto(), KeyMods::empty());

        let all = FfiKeyMods {
            shift: true,
            ctrl: true,
            alt: true,
            command: true,
        };
        assert_eq!(
            all.to_proto(),
            KeyMods::SHIFT | KeyMods::CTRL | KeyMods::ALT | KeyMods::SUPER
        );

        let ctrl = FfiKeyMods {
            shift: false,
            ctrl: true,
            alt: false,
            command: false,
        };
        assert_eq!(ctrl.to_proto(), KeyMods::CTRL);
    }

    #[test]
    fn pane_label_falls_back_to_index() {
        assert_eq!(pane_label(0, ""), "pane 1");
        assert_eq!(pane_label(3, "   "), "pane 4");
        assert_eq!(pane_label(0, "vim"), "vim");
    }

    #[test]
    fn tab_label_falls_back_to_one_based_index() {
        assert_eq!(tab_label(0, "", ""), "1");
        assert_eq!(tab_label(2, "   ", "   "), "3");
        assert_eq!(tab_label(0, "build", "vim"), "build");
        assert_eq!(tab_label(0, "", "vim"), "vim");
    }

    #[test]
    fn toggle_pause_action_maps_to_core_action() {
        assert!(matches!(
            Action::from(FfiAction::TogglePause),
            Action::TogglePause
        ));
        assert!(matches!(
            Action::from(FfiAction::ToggleFocusedPaneNoAutoPause),
            Action::ToggleFocusedPaneNoAutoPause
        ));
        assert!(matches!(
            Action::from(FfiAction::ToggleActiveSessionNoAutoPause),
            Action::ToggleActiveSessionNoAutoPause
        ));
    }

    #[test]
    fn pause_state_maps_from_pause_reason() {
        assert_eq!(
            FfiPauseState::from(PauseReason::None),
            FfiPauseState::Active
        );
        assert_eq!(
            FfiPauseState::from(PauseReason::Manual),
            FfiPauseState::PausedManual
        );
        assert_eq!(
            FfiPauseState::from(PauseReason::Auto),
            FfiPauseState::PausedBackground
        );
    }

    #[test]
    fn abi_version_export_matches_constant() {
        // The exported free fn must return the constant verbatim. Asserting the
        // invariant (not a hardcoded number) keeps `KMUX_FFI_ABI_VERSION` the
        // single source of truth: bumping it needs no edit here, in the Swift
        // app, or in CI. uniffi's regenerated binding-checksum check is what
        // actually guards against stale bindings/dylib drift.
        assert_eq!(kmux_ffi_abi_version(), KMUX_FFI_ABI_VERSION);
    }

    #[test]
    fn launch_row_flattens_each_variant() {
        // Local new + add-remote bookends.
        let r = launch_row_to_ffi(LaunchRow::LocalNewSession {
            default_cwd: "/home/u".into(),
        });
        assert_eq!(r.kind, FfiLaunchRowKind::LocalNewSession);
        assert_eq!(r.detail, "/home/u");

        // A remote header carries its peer, status, and expansion.
        let r = launch_row_to_ffi(LaunchRow::Remote {
            peer: "alice@box".into(),
            label: "alice@box".into(),
            status: RemoteStatus::Error("nope".into()),
            expanded: true,
        });
        assert_eq!(r.kind, FfiLaunchRowKind::Remote);
        assert_eq!(r.peer.as_deref(), Some("alice@box"));
        assert_eq!(r.status, FfiRemoteStatus::Error);
        assert_eq!(r.detail, "nope"); // the error reason surfaces as detail
        assert!(r.expanded);

        // An existing remote session carries both routing keys.
        let r = launch_row_to_ffi(LaunchRow::RemoteExisting {
            peer: "alice@box".into(),
            word_id: "eagle".into(),
            name: "proj".into(),
            cwd: "/srv".into(),
            active: true,
        });
        assert_eq!(r.kind, FfiLaunchRowKind::RemoteExisting);
        assert_eq!(r.peer.as_deref(), Some("alice@box"));
        assert_eq!(r.word_id.as_deref(), Some("eagle"));
        assert!(r.active);
    }

    #[test]
    fn mode_to_ffi_maps_launcher_modes() {
        assert!(matches!(
            mode_to_ffi(&Mode::LaunchPicker),
            FfiMode::LaunchPicker
        ));
        assert!(matches!(mode_to_ffi(&Mode::AddRemote), FfiMode::AddRemote));
        assert!(matches!(
            mode_to_ffi(&Mode::RemoteNewSession { peer: "alice@box".into() }),
            FfiMode::RemoteNewSession { peer } if peer == "alice@box"
        ));
    }

    #[test]
    fn mode_to_ffi_maps_session_close_confirmation() {
        assert!(matches!(
            mode_to_ffi(&Mode::ConfirmCloseSession {
                word_id: "eagle".into(),
                name: "build".into(),
            }),
            FfiMode::ConfirmCloseSession { word_id, name }
                if word_id == "eagle" && name == "build"
        ));
    }

    #[test]
    fn ffi_appearance_maps_fields() {
        use kmux_app::appearance::FontFeature;
        let appearance = Appearance {
            family: "JetBrains Mono".into(),
            family_bold: Some("JetBrains Mono Bold".into()),
            family_italic: None,
            family_bold_italic: None,
            size_pt: 14.0,
            style: Some("Medium".into()),
            features: vec![
                FontFeature {
                    tag: "ss01".into(),
                    value: 1,
                },
                FontFeature {
                    tag: "liga".into(),
                    value: 0,
                },
            ],
            cell_width_adjust: CellAdjust::Pixels(2.0),
            cell_height_adjust: CellAdjust::Percent(10.0),
        };
        let ffi = FfiAppearance::from(&appearance);
        assert_eq!(ffi.family, "JetBrains Mono");
        assert_eq!(ffi.family_bold.as_deref(), Some("JetBrains Mono Bold"));
        assert_eq!(ffi.size_pt, 14.0);
        assert_eq!(ffi.style.as_deref(), Some("Medium"));
        assert_eq!(
            ffi.features,
            vec!["ss01=1".to_string(), "liga=0".to_string()]
        );
        assert!(matches!(
            ffi.cell_width_adjust,
            FfiCellAdjust {
                kind: FfiCellAdjustKind::Pixels,
                value
            } if value == 2.0
        ));
        assert!(matches!(
            ffi.cell_height_adjust,
            FfiCellAdjust {
                kind: FfiCellAdjustKind::Percent,
                value
            } if value == 10.0
        ));
    }

    #[test]
    fn tiling_actions_map_to_core_actions() {
        assert_eq!(Action::from(FfiAction::SplitRight), Action::SplitRight);
        assert_eq!(Action::from(FfiAction::FocusLeft), Action::FocusLeft);
        assert_eq!(Action::from(FfiAction::ResizeDown), Action::ResizeDown);
        assert_eq!(Action::from(FfiAction::SwapNext), Action::SwapNext);
        assert_eq!(Action::from(FfiAction::RenameTab), Action::RenameTab);
        assert_eq!(Action::from(FfiAction::CloseTab), Action::CloseTab);
        assert_eq!(Action::from(FfiAction::CycleLayout), Action::CycleLayout);
        assert_eq!(Action::from(FfiAction::ToggleZoom), Action::ToggleZoom);
        assert_eq!(
            Action::from(FfiAction::FocusPaneAt { index: 2 }),
            Action::FocusPaneAt(2)
        );
        assert_eq!(Action::from(FfiAction::UndoClose), Action::UndoClose);
        assert_eq!(
            Action::from(FfiAction::ToggleConnection),
            Action::ToggleConnection
        );
    }

    #[test]
    fn divider_round_trips_through_ffi() {
        let d = kmux_app::layout::Divider {
            path: vec![1, 0],
            dir: SplitDir::Vertical,
            before: 1,
            hit_col: 3,
            hit_row: 12,
            hit_cols: 40,
            hit_rows: 1,
            pair_start: 6,
            pair_len: 24,
        };
        let back = FfiDivider::from_layout(d.clone()).into_layout();
        assert_eq!(back, d);
        // The Horizontal/Vertical ⇄ vertical_bar mapping is faithful.
        assert!(!FfiDivider::from_layout(d.clone()).vertical_bar);
        let h = kmux_app::layout::Divider {
            dir: SplitDir::Horizontal,
            ..d
        };
        assert!(FfiDivider::from_layout(h).vertical_bar);
    }
}
