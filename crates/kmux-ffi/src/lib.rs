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
//!   packed byte buffer (see [`cells`]) so the renderer copies only changed
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

mod cells;

use std::sync::{Arc, Mutex};

use tokio::runtime::Runtime;

use kmux_app::cmd;
use kmux_app::config;
use kmux_app::core::{AppCore, TopBarAction};
use kmux_app::driver::{FrontendDriver, FrontendEffect};
use kmux_app::mode::{Action, CommandState, Mode};
use kmux_app::subcommands::parse_target;
use kmux_app::theme::{self, Rgb, Theme};
use kmux_client::connection_state::ConnectionState;
use kmux_client::generate_instance_id;
use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::{char_to_proto_key, encode_mouse_scroll};
use kmux_protocol::messages::{
    ClientCapabilities, KeyAction, KeyCode, KeyEvent, KeyMods, TermSize,
};

uniffi::setup_scaffolding!();

/// ABI version of this FFI surface. Bumped on any breaking change to the
/// exported types/functions, mirroring the repo's other versioned boundaries
/// (`kmux-ghostty-sys`'s `EXPECTED_ABI_VERSION`, the wire protocol version).
/// The Swift wrapper asserts this on startup, on top of uniffi's built-in
/// binding-checksum check.
pub const KMUX_FFI_ABI_VERSION: u32 = 3;

/// Returns [`KMUX_FFI_ABI_VERSION`]. A free function so the Swift wrapper can
/// check it before constructing a driver.
#[uniffi::export]
pub fn kmux_ffi_abi_version() -> u32 {
    KMUX_FFI_ABI_VERSION
}

/// Failure constructing a [`KmuxDriver`].
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("failed to initialize kmux: {message}")]
    Init { message: String },
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
    CopyToClipboard { text: String },
    RequestPaste,
    Quit,
}

impl From<FrontendEffect> for FfiEffect {
    fn from(e: FrontendEffect) -> Self {
        match e {
            FrontendEffect::NeedsRender => FfiEffect::NeedsRender,
            FrontendEffect::ForceClear => FfiEffect::ForceClear,
            FrontendEffect::PaletteChanged => FfiEffect::PaletteChanged,
            FrontendEffect::CopyToClipboard(text) => FfiEffect::CopyToClipboard { text },
            FrontendEffect::RequestPaste => FfiEffect::RequestPaste,
            FrontendEffect::Quit => FfiEffect::Quit,
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
    JumpToSession { index: u32 },
    CreatePane,
    ClosePane,
    NextPane,
    PrevPane,
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
    ScrollUp { lines: u32 },
    ScrollDown { lines: u32 },
    ScrollPageUp,
    ScrollPageDown,
    ToggleHud,
    ToggleMetrics,
    ToggleInputLock,
    CopySelection,
    Paste,
    Quit,
    Reconnect,
}

impl From<FfiAction> for Action {
    fn from(a: FfiAction) -> Self {
        match a {
            FfiAction::CreateSession => Action::CreateSession,
            FfiAction::CloseSession => Action::CloseSession,
            FfiAction::NextSession => Action::NextSession,
            FfiAction::PrevSession => Action::PrevSession,
            FfiAction::JumpToSession { index } => Action::JumpToSession(index as usize),
            FfiAction::CreatePane => Action::CreatePane,
            FfiAction::ClosePane => Action::ClosePane,
            FfiAction::NextPane => Action::NextPane,
            FfiAction::PrevPane => Action::PrevPane,
            FfiAction::CloseTab => Action::CloseTab,
            FfiAction::RenameTab => Action::RenameTab,
            FfiAction::SplitRight => Action::SplitRight,
            FfiAction::SplitDown => Action::SplitDown,
            FfiAction::FocusLeft => Action::FocusLeft,
            FfiAction::FocusRight => Action::FocusRight,
            FfiAction::FocusUp => Action::FocusUp,
            FfiAction::FocusDown => Action::FocusDown,
            FfiAction::ResizeLeft => Action::ResizeLeft,
            FfiAction::ResizeRight => Action::ResizeRight,
            FfiAction::ResizeUp => Action::ResizeUp,
            FfiAction::ResizeDown => Action::ResizeDown,
            FfiAction::SwapNext => Action::SwapNext,
            FfiAction::SwapPrev => Action::SwapPrev,
            FfiAction::ScrollUp { lines } => Action::ScrollUp(lines as usize),
            FfiAction::ScrollDown { lines } => Action::ScrollDown(lines as usize),
            FfiAction::ScrollPageUp => Action::ScrollPageUp,
            FfiAction::ScrollPageDown => Action::ScrollPageDown,
            FfiAction::ToggleHud => Action::ToggleHud,
            FfiAction::ToggleMetrics => Action::ToggleMetrics,
            FfiAction::ToggleInputLock => Action::ToggleInputLock,
            FfiAction::CopySelection => Action::CopySelection,
            FfiAction::Paste => Action::Paste,
            FfiAction::Quit => Action::Quit,
            FfiAction::Reconnect => Action::Reconnect,
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
            FfiNamedKey::Enter => KeyCode::Enter,
            FfiNamedKey::Tab => KeyCode::Tab,
            FfiNamedKey::Backspace => KeyCode::Backspace,
            FfiNamedKey::Escape => KeyCode::Escape,
            FfiNamedKey::ArrowUp => KeyCode::ArrowUp,
            FfiNamedKey::ArrowDown => KeyCode::ArrowDown,
            FfiNamedKey::ArrowLeft => KeyCode::ArrowLeft,
            FfiNamedKey::ArrowRight => KeyCode::ArrowRight,
            FfiNamedKey::PageUp => KeyCode::PageUp,
            FfiNamedKey::PageDown => KeyCode::PageDown,
            FfiNamedKey::Home => KeyCode::Home,
            FfiNamedKey::End => KeyCode::End,
            FfiNamedKey::Delete => KeyCode::Delete,
            FfiNamedKey::Insert => KeyCode::Insert,
            FfiNamedKey::F1 => KeyCode::F1,
            FfiNamedKey::F2 => KeyCode::F2,
            FfiNamedKey::F3 => KeyCode::F3,
            FfiNamedKey::F4 => KeyCode::F4,
            FfiNamedKey::F5 => KeyCode::F5,
            FfiNamedKey::F6 => KeyCode::F6,
            FfiNamedKey::F7 => KeyCode::F7,
            FfiNamedKey::F8 => KeyCode::F8,
            FfiNamedKey::F9 => KeyCode::F9,
            FfiNamedKey::F10 => KeyCode::F10,
            FfiNamedKey::F11 => KeyCode::F11,
            FfiNamedKey::F12 => KeyCode::F12,
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

/// The active grid as a packed cell buffer (see [`cells`]) plus dimensions and
/// cursor. `cells` is `rows * cols * 16` bytes, row-major.
#[derive(uniffi::Record)]
pub struct GridSnapshot {
    pub rows: u32,
    pub cols: u32,
    pub cursor: FfiCursor,
    pub cells: Vec<u8>,
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
            ConnectionState::Idle => FfiConnStatus::Idle,
            ConnectionState::Handshaking => FfiConnStatus::Handshaking,
            ConnectionState::Connected { .. } => FfiConnStatus::Connected,
            ConnectionState::Reconnecting { .. } => FfiConnStatus::Reconnecting,
            ConnectionState::Disconnected { .. } => FfiConnStatus::Disconnected,
        }
    }
}

/// Connection state + a human-readable badge label.
#[derive(uniffi::Record)]
pub struct FfiConnInfo {
    pub status: FfiConnStatus,
    pub label: String,
}

/// One session in the session list.
#[derive(uniffi::Record)]
pub struct FfiSession {
    pub word_id: String,
    pub name: String,
    pub cwd: String,
    pub active: bool,
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
}

/// Tab name, falling back to its 1-based index (mirrors the client's
/// `tab_label`).
fn tab_label(index: u32, name: &str) -> String {
    if name.trim().is_empty() {
        format!("{}", index + 1)
    } else {
        name.to_string()
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
    Server,
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
}

/// Which interaction mode / overlay is active. Carries the text the matching
/// overlay needs (connecting label, disconnect reason); list contents are read
/// via the dedicated getters.
#[derive(uniffi::Enum)]
pub enum FfiMode {
    Normal,
    Locked,
    SessionPicker,
    ServerPicker,
    DirectoryPicker,
    Help,
    Command,
    Connecting { label: String },
    Disconnected { reason: String },
    Other,
}

fn mode_to_ffi(mode: &Mode) -> FfiMode {
    match mode {
        Mode::Normal => FfiMode::Normal,
        Mode::Locked => FfiMode::Locked,
        Mode::SessionPicker => FfiMode::SessionPicker,
        Mode::ServerPicker => FfiMode::ServerPicker,
        Mode::DirectoryPicker => FfiMode::DirectoryPicker,
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
fn build_core(config: &DriverConfig) -> AppCore {
    let (target, parsed_server) = parse_target(config.server.as_deref(), config.ssh_port);
    let auto_cwd = config
        .cwd
        .clone()
        .or_else(|| parsed_server.as_ref().and_then(|p| p.path.clone()));
    let theme = config::resolve_theme(config.theme.as_deref());
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();
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
        generate_instance_id(),
        config.session.clone(),
        auto_cwd,
        capabilities,
        theme,
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
        let rt = Runtime::new().map_err(|e| FfiError::Init {
            message: e.to_string(),
        })?;
        let core = build_core(&config);
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
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.tick().into_iter().map(FfiEffect::from).collect()
    }

    /// Dispatch a curated action; returns any resulting effects. Reconnect /
    /// server-switch are applied internally by the driver.
    pub fn dispatch(&self, action: FfiAction) -> Vec<FfiEffect> {
        let act = Action::from(action);
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        // `dispatch_action` is async; `block_on` drives it to completion and
        // provides the runtime context its internal spawns need. (Not combined
        // with `rt.enter()`, which would make `block_on` panic.)
        self.rt
            .block_on(d.dispatch_action(act))
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
        let cells = cells::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: cells::cursor_shape_code(c.shape),
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

    /// The current connection state + badge label.
    pub fn connection(&self) -> FfiConnInfo {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let state = d.mgr.connection_state();
        FfiConnInfo {
            status: FfiConnStatus::from(state),
            label: state.badge_label(),
        }
    }

    /// The session list, with the active session flagged.
    pub fn sessions(&self) -> Vec<FfiSession> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_session().map(|s| s.to_string());
        d.mgr
            .session_list()
            .iter()
            .map(|e| FfiSession {
                active: active.as_deref() == Some(e.meta.word_id.as_str()),
                word_id: e.meta.word_id.clone(),
                name: e.meta.name.clone(),
                cwd: e.meta.cwd.clone(),
            })
            .collect()
    }

    /// The panes (tabs) of the active session, with the active pane flagged.
    pub fn panes(&self) -> Vec<FfiPane> {
        let d = self.inner.lock().expect("driver mutex poisoned");
        let active = d.mgr.active_pane_id().map(|s| s.to_string());
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
        d.mgr
            .active_session_tabs()
            .iter()
            .map(|t| FfiTab {
                tab_index: t.tab_index,
                name: tab_label(t.tab_index, &t.name),
                active: active == Some(t.tab_index),
            })
            .collect()
    }

    /// View a tab of the active session by index (a tab-strip click): attaches
    /// its pane set and focuses its pane. Signals a render.
    pub fn select_tab(&self, tab_index: u32) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.select_tab(tab_index);
        core.needs_render = true;
        vec![FfiEffect::NeedsRender]
    }

    /// Focus a tiled pane by id within the active tab (a click on a tile, or a
    /// keyboard focus move resolved frontend-side). Publishes the shared focus to
    /// the server. Signals a render.
    pub fn focus_pane(&self, pane_id: String) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.focus_pane(pane_id);
        core.needs_render = true;
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
        let Some(word) = d.mgr.active_session().map(|s| s.to_string()) else {
            return Vec::new();
        };
        let focused = d
            .mgr
            .active_pane_id()
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(_, i)| i.parse::<u32>().ok());
        let Some(layout) = d.mgr.active_layout() else {
            return Vec::new();
        };
        kmux_app::layout::resolve_layout(
            layout,
            area_cols,
            area_rows,
            &kmux_app::layout::LayoutConfig::default(),
        )
        .into_iter()
        .map(|r| FfiPaneRect {
            pane_id: format!("{word}/{}", r.pane_index),
            pane_index: r.pane_index,
            col: r.col as u32,
            row: r.row as u32,
            cols: r.cols as u32,
            rows: r.rows as u32,
            focused: focused == Some(r.pane_index),
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
        let cells = cells::encode_cells(grid, &d.palette);
        let c = grid.cursor();
        Some(GridSnapshot {
            rows: grid.rows as u32,
            cols: grid.cols as u32,
            cursor: FfiCursor {
                row: c.row as u32,
                col: c.col as u32,
                shape: cells::cursor_shape_code(c.shape),
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
        let Some(pane_id) = d.mgr.active_pane_id().map(|s| s.to_string()) else {
            return;
        };
        let use_pty = d
            .mgr
            .buffer(&pane_id)
            .map(|g| g.modes().mouse_report())
            .unwrap_or(false);
        if use_pty {
            let sgr = d
                .mgr
                .buffer(&pane_id)
                .map(|g| g.modes().sgr_mouse())
                .unwrap_or(false);
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
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.core_mut().mode = Mode::Command(CommandState {
            buffer: input.clone(),
            cursor: input.len(),
            ..CommandState::default()
        });
        self.rt
            .block_on(d.dispatch_action(Action::CommandSubmit))
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
            Mode::ServerPicker => {
                let entries = core
                    .filtered_servers()
                    .into_iter()
                    .map(|s| {
                        // time_ago() borrows all of `s`, so compute it before
                        // moving `display` out.
                        let detail = s.time_ago();
                        FfiPickerEntry {
                            label: s.display,
                            detail,
                        }
                    })
                    .collect();
                Some(FfiPicker {
                    kind: FfiPickerKind::Server,
                    query: core.server_picker_search.clone(),
                    selected: core.server_picker_selected as u32,
                    entries,
                })
            }
            Mode::DirectoryPicker => {
                let entries = core
                    .dir_picker_matches()
                    .into_iter()
                    .map(|e| FfiPickerEntry {
                        label: core.mgr.display_name_for(&e.meta.word_id),
                        detail: e.meta.cwd.clone(),
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

    /// Open the recent-servers picker.
    pub fn open_server_picker(&self) -> Vec<FfiEffect> {
        let _guard = self.rt.enter();
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        d.apply_top_bar_action(TopBarAction::OpenServerPicker)
            .into_iter()
            .map(FfiEffect::from)
            .collect()
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
        core.needs_render = true;
    }

    /// Set the open picker's highlighted row (hover/click).
    pub fn set_picker_selected(&self, index: u32) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.set_picker_selected(index as usize);
        core.needs_render = true;
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

    /// Submit the directory picker's typed path: select the matching session or
    /// create a new one at that path (the create-from-typed-path path that a
    /// plain `activate_picker` click does not cover).
    pub fn submit_directory(&self) -> Vec<FfiEffect> {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        self.rt
            .block_on(d.dispatch_action(Action::DirPickerSubmit))
            .into_iter()
            .map(FfiEffect::from)
            .collect()
    }

    /// Close any open picker / overlay (back to normal interaction).
    pub fn cancel_picker(&self) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mode = Mode::Normal;
        core.needs_render = true;
    }

    /// Rename a session by word id (trims surrounding whitespace).
    pub fn rename_session(&self, word_id: String, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.rename_session(&word_id, name.trim());
        core.needs_render = true;
    }

    /// Close a session by word id.
    pub fn close_session(&self, word_id: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        let core = d.core_mut();
        core.mgr.close_session(&word_id);
        core.needs_render = true;
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
        }
    }

    /// The built-in theme names (for a Preferences theme picker).
    pub fn available_themes(&self) -> Vec<String> {
        theme::BUILTIN_THEMES
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Switch the active palette to a built-in theme by name (no-op if unknown).
    /// The driver emits `PaletteChanged` from the next [`tick`](Self::tick).
    pub fn set_theme(&self, name: String) {
        let mut d = self.inner.lock().expect("driver mutex poisoned");
        if let Some(t) = theme::builtin_theme(&name) {
            let core = d.core_mut();
            core.palette = t;
            core.needs_render = true;
        }
    }

    /// Whether the cursor is shown on the current frame (blink phase).
    pub fn blink_on(&self) -> bool {
        self.inner.lock().expect("driver mutex poisoned").blink_on()
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(tab_label(0, ""), "1");
        assert_eq!(tab_label(2, "   "), "3");
        assert_eq!(tab_label(0, "build"), "build");
    }

    #[test]
    fn abi_version_is_three() {
        // The tiling surface (tabs/layout/per-pane grid/new actions) bumped the
        // ABI; the Swift wrapper asserts the same constant on startup.
        assert_eq!(KMUX_FFI_ABI_VERSION, 3);
        assert_eq!(kmux_ffi_abi_version(), 3);
    }

    #[test]
    fn tiling_actions_map_to_core_actions() {
        assert_eq!(Action::from(FfiAction::SplitRight), Action::SplitRight);
        assert_eq!(Action::from(FfiAction::FocusLeft), Action::FocusLeft);
        assert_eq!(Action::from(FfiAction::ResizeDown), Action::ResizeDown);
        assert_eq!(Action::from(FfiAction::SwapNext), Action::SwapNext);
        assert_eq!(Action::from(FfiAction::RenameTab), Action::RenameTab);
        assert_eq!(Action::from(FfiAction::CloseTab), Action::CloseTab);
    }
}
