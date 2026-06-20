//! Safe Rust façade over [`kmux-ghostty-sys`]. Wraps the kmux-owned C ABI
//! exposed by `libkmux_ghostty` (a Zig wrapper around libghostty-vt v1.3.1)
//! in idiomatic, lifetime-checked types.
//!
//! # Boundaries
//!
//! The Zig side owns a single `Terminal` + `Stream` per [`GhosttyTerm`] and
//! speaks a narrow C ABI (see `kmux-ghostty-sys`). This crate:
//! - copies cells into caller-owned [`CellState`] buffers (no allocations
//!   crossing FFI);
//! - adapts callbacks into a safe [`EventSink`] trait object;
//! - checks the ABI version once, on construction;
//! - asserts `Send` at compile-time so kmuxd's per-pane `Arc<Mutex<…>>` can
//!   move the handle between Tokio tasks without runtime guards.
//!
//! # Thread safety
//!
//! [`GhosttyTerm`] is `Send` but **not** `Sync`: at most one caller may mutate
//! or read the terminal at a time. kmuxd wraps it in `Arc<Mutex<DiffEngine<…>>>`
//! to serialise access. Event callbacks fire synchronously inside [`feed`] —
//! they borrow data from the Zig side and must not retain it past return.

#![deny(missing_debug_implementations)]

use core::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use kmux_protocol::messages::{CellColor, CellState, CursorShape, CursorState, TermModes};
use static_assertions::assert_impl_all;
use static_assertions::assert_not_impl_any;
use thiserror::Error;

use kmux_ghostty_sys as sys;

pub use kmux_ghostty_sys::EXPECTED_ABI_VERSION;

/// ABI version reported by `libkmux_ghostty` at link time.
#[must_use]
pub fn abi_version() -> u32 {
    unsafe { sys::kmux_ghostty_abi_version() }
}

/// Panic if the Zig-side ABI version does not match [`EXPECTED_ABI_VERSION`].
pub fn check_abi_version() {
    let got = abi_version();
    assert_eq!(
        got, EXPECTED_ABI_VERSION,
        "libkmux_ghostty ABI mismatch: linked version is {got}, \
         but this crate expects {EXPECTED_ABI_VERSION}. \
         Rebuild with `cargo clean -p kmux-ghostty-sys`.",
    );
}

/// Grid size (rows × cols) plus an optional pixel-size hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl From<TermSize> for sys::KmuxSize {
    fn from(t: TermSize) -> Self {
        Self {
            rows: t.rows,
            cols: t.cols,
            pixel_width: t.pixel_width,
            pixel_height: t.pixel_height,
        }
    }
}

impl From<sys::KmuxSize> for TermSize {
    fn from(s: sys::KmuxSize) -> Self {
        Self {
            rows: s.rows,
            cols: s.cols,
            pixel_width: s.pixel_width,
            pixel_height: s.pixel_height,
        }
    }
}

/// OSC 9;4 (ConEmu / Windows-Terminal) progress-report state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// Clear any progress indication (`OSC 9;4;0`).
    Remove,
    /// Normal progress (`OSC 9;4;1;<pct>`).
    Set,
    /// Error / failed (`OSC 9;4;2;<pct>`).
    Error,
    /// Indeterminate / busy with no known percentage (`OSC 9;4;3`).
    Indeterminate,
    /// Paused / warning (`OSC 9;4;4;<pct>`).
    Pause,
}

/// A single OSC 9;4 progress report. `progress` is `0..=100` when the sequence
/// carried a value, or `None` for the value-less states (remove/indeterminate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressReport {
    pub state: ProgressState,
    pub progress: Option<u8>,
}

impl ProgressReport {
    /// The default "no progress bar" report.
    #[must_use]
    pub fn none() -> Self {
        Self {
            state: ProgressState::Remove,
            progress: None,
        }
    }

    /// Decode the raw `(state, value, has)` triple the C ABI delivers.
    fn from_raw(state: u8, value: u8, has: u8) -> Self {
        let state = match state {
            0 => ProgressState::Remove,
            1 => ProgressState::Set,
            2 => ProgressState::Error,
            3 => ProgressState::Indeterminate,
            _ => ProgressState::Pause,
        };
        Self {
            state,
            progress: if has != 0 { Some(value) } else { None },
        }
    }
}

/// Terminal-emitted events. Callbacks fire **synchronously** inside
/// [`GhosttyTerm::feed`]; implementations must not block or retain pointers.
pub trait EventSink: Send + Sync + 'static {
    fn on_title(&self, _title: &str) {}
    fn on_bell(&self) {}
    /// OSC 52 clipboard write. `selection` is a single byte (`c`/`p`/`s`/...)
    /// identifying the selection target; `base64` is the still-encoded payload
    /// (the Zig side does not decode).
    fn on_osc52(&self, _selection: u8, _base64: &[u8]) {}
    /// OSC 8 start-hyperlink. `id` is the optional URL id (empty `""` is
    /// normalised to `None`); `uri` is the target.
    fn on_hyperlink(&self, _id: Option<&str>, _uri: &str) {}
    /// OSC 9;4 progress report (ConEmu/WT progress bar). Fired on each change.
    fn on_progress(&self, _report: ProgressReport) {}
}

/// A no-op sink useful for construction sites that do not route events
/// (e.g. unit tests, snapshot tooling).
#[derive(Debug, Default)]
pub struct NullSink;
impl EventSink for NullSink {}

#[derive(Debug, Error)]
pub enum GhosttyError {
    #[error("allocation failed inside libkmux_ghostty")]
    Alloc,
    #[error("invalid terminal size (rows/cols must be non-zero)")]
    InvalidSize,
    #[error("VT parser returned an error while feeding bytes")]
    Feed,
    #[error("terminal resize failed inside libkmux_ghostty")]
    Resize,
    #[error("buffer too small for the requested grid ({need} cells, got {got})")]
    BadBuffer { need: usize, got: usize },
}

fn map_rc(rc: i32) -> Result<(), GhosttyError> {
    match rc {
        sys::KMUX_OK => Ok(()),
        sys::KMUX_ERR_ALLOC => Err(GhosttyError::Alloc),
        sys::KMUX_ERR_INVALID_SIZE => Err(GhosttyError::InvalidSize),
        sys::KMUX_ERR_FEED => Err(GhosttyError::Feed),
        sys::KMUX_ERR_RESIZE => Err(GhosttyError::Resize),
        sys::KMUX_ERR_BAD_BUFFER => Err(GhosttyError::BadBuffer { need: 0, got: 0 }),
        other => panic!("libkmux_ghostty returned unexpected error code {other}"),
    }
}

/// Bridge that owns the `Arc<dyn EventSink>` and exposes it to the Zig side
/// through stable C-compatible trampolines.
///
/// The `Box<EventBridge>` is pinned for the lifetime of the [`GhosttyTerm`];
/// the Zig wrapper stores a copy of [`sys::KmuxEventSink`] whose `user` field
/// points at the `EventBridge` here.
struct EventBridge {
    sink: Arc<dyn EventSink>,
}

impl EventBridge {
    fn new(sink: Arc<dyn EventSink>) -> Box<Self> {
        Box::new(Self { sink })
    }

    fn as_c_sink(self: &mut Box<Self>) -> sys::KmuxEventSink {
        let user = self.as_mut() as *mut Self as *mut c_void;
        sys::KmuxEventSink {
            user,
            on_title: Some(trampoline_title),
            on_bell: Some(trampoline_bell),
            on_osc52: Some(trampoline_osc52),
            on_hyperlink: Some(trampoline_hyperlink),
            on_progress: Some(trampoline_progress),
        }
    }
}

// Safety: `EventBridge` is move-only and holds an `Arc<dyn EventSink>` where
// `EventSink: Send + Sync`. Sending the bridge across threads is equivalent
// to sending the Arc.
unsafe impl Send for EventBridge {}

unsafe extern "C" fn trampoline_title(user: *mut c_void, ptr: *const u8, len: usize) {
    // SAFETY: `user` was produced from `Box::as_mut()`, still alive because
    // `GhosttyTerm` owns the `Box`. The Zig side passes a borrowed UTF-8
    // slice; we copy via `from_utf8` before handing it to the sink.
    let bridge = unsafe { &*(user as *const EventBridge) };
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    if let Ok(s) = std::str::from_utf8(bytes) {
        bridge.sink.on_title(s);
    }
}

unsafe extern "C" fn trampoline_bell(user: *mut c_void) {
    let bridge = unsafe { &*(user as *const EventBridge) };
    bridge.sink.on_bell();
}

unsafe extern "C" fn trampoline_osc52(
    user: *mut c_void,
    selection: u8,
    ptr: *const u8,
    len: usize,
) {
    let bridge = unsafe { &*(user as *const EventBridge) };
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    bridge.sink.on_osc52(selection, bytes);
}

unsafe extern "C" fn trampoline_hyperlink(
    user: *mut c_void,
    id_ptr: *const u8,
    id_len: usize,
    uri_ptr: *const u8,
    uri_len: usize,
) {
    let bridge = unsafe { &*(user as *const EventBridge) };
    let id_bytes = unsafe { std::slice::from_raw_parts(id_ptr, id_len) };
    let uri_bytes = unsafe { std::slice::from_raw_parts(uri_ptr, uri_len) };
    let (Ok(id), Ok(uri)) = (
        std::str::from_utf8(id_bytes),
        std::str::from_utf8(uri_bytes),
    ) else {
        return;
    };
    let id_opt = if id.is_empty() { None } else { Some(id) };
    bridge.sink.on_hyperlink(id_opt, uri);
}

unsafe extern "C" fn trampoline_progress(user: *mut c_void, state: u8, value: u8, has: u8) {
    let bridge = unsafe { &*(user as *const EventBridge) };
    bridge
        .sink
        .on_progress(ProgressReport::from_raw(state, value, has));
}

/// Owned handle to a libghostty-vt terminal. One per kmux pane.
///
/// Construction checks the ABI version once and panics on mismatch; all
/// subsequent FFI calls trust the version. Dropping a `GhosttyTerm` calls
/// `kmux_ghostty_free` and releases the `EventBridge` in that order.
pub struct GhosttyTerm {
    handle: NonNull<sys::kmux_ghostty_term>,
    // Drop order: `_bridge` is dropped after `handle` (declared after) —
    // but Rust drops fields top-to-bottom by declaration. We therefore
    // declare `handle` first so it is freed before `_bridge`. The explicit
    // `Drop` impl for `GhosttyTerm` enforces this independently of field
    // order by calling `kmux_ghostty_free` before the `Box` is dropped.
    _bridge: Box<EventBridge>,
}

// Safety: the Zig wrapper is single-threaded (no internal synchronisation);
// `GhosttyTerm` may move between threads but only one thread may hold a
// `&mut GhosttyTerm` at a time. This matches `Arc<Mutex<DiffEngine<…>>>` in
// kmuxd. We therefore implement `Send` but deliberately do NOT implement
// `Sync`.
unsafe impl Send for GhosttyTerm {}

// Compile-time verification. Keeping these here ensures refactors that add a
// !Send field (e.g. `Rc`, a raw `*mut` wrapped in a local type) fail the
// build instead of surfacing as UB later.
assert_impl_all!(GhosttyTerm: Send);
assert_not_impl_any!(GhosttyTerm: Sync);

impl std::fmt::Debug for GhosttyTerm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhosttyTerm")
            .field("handle", &self.handle)
            .finish()
    }
}

impl GhosttyTerm {
    /// Construct a new terminal of the given size, routing events through
    /// `sink`. `scrollback` caps the number of retained history rows.
    pub fn new(
        size: TermSize,
        scrollback: u32,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self, GhosttyError> {
        check_abi_version();
        let mut bridge = EventBridge::new(sink);
        let c_sink = bridge.as_c_sink();
        let c_size: sys::KmuxSize = size.into();
        let mut handle: *mut sys::kmux_ghostty_term = std::ptr::null_mut();
        let rc = unsafe { sys::kmux_ghostty_new(&c_size, scrollback, &c_sink, &mut handle) };
        map_rc(rc)?;
        let handle = NonNull::new(handle).ok_or(GhosttyError::Alloc)?;
        Ok(Self {
            handle,
            _bridge: bridge,
        })
    }

    /// Feed a byte slice into the VT parser. Event callbacks fire
    /// synchronously inside this call.
    pub fn feed(&mut self, data: &[u8]) -> Result<(), GhosttyError> {
        let rc = unsafe { sys::kmux_ghostty_feed(self.handle.as_ptr(), data.as_ptr(), data.len()) };
        map_rc(rc)
    }

    /// Current grid dimensions plus pixel-size hint.
    #[must_use]
    pub fn size(&self) -> TermSize {
        let mut s = sys::KmuxSize::default();
        unsafe { sys::kmux_ghostty_size(self.handle.as_ptr(), &mut s) };
        s.into()
    }

    /// Resize the grid. Preserves scrollback; alt-screen is handled by Ghostty.
    pub fn resize(&mut self, size: TermSize) -> Result<(), GhosttyError> {
        let c_size: sys::KmuxSize = size.into();
        let rc = unsafe { sys::kmux_ghostty_resize(self.handle.as_ptr(), &c_size) };
        map_rc(rc)
    }

    /// Populate `out` with the active-screen grid. `out` must hold at least
    /// `rows * cols` entries; extra capacity is left untouched.
    pub fn fill_cells(&self, out: &mut [CellState]) -> Result<(), GhosttyError> {
        let size = self.size();
        let need = size.rows as usize * size.cols as usize;
        if out.len() < need {
            return Err(GhosttyError::BadBuffer {
                need,
                got: out.len(),
            });
        }
        let mut raw = vec![sys::KmuxCell::default(); need];
        let rc =
            unsafe { sys::kmux_ghostty_fill_cells(self.handle.as_ptr(), raw.as_mut_ptr(), need) };
        map_rc(rc)?;
        for (dst, src) in out.iter_mut().zip(raw.iter()).take(need) {
            *dst = convert_cell(src);
        }
        Ok(())
    }

    /// Combined grid + cursor + modes read in one FFI crossing. Preferred on
    /// the hot path where [`DiffEngine`] reads all three together.
    pub fn fill_cells_and_cursor(
        &self,
        out: &mut [CellState],
    ) -> Result<(CursorState, TermModes), GhosttyError> {
        let size = self.size();
        let need = size.rows as usize * size.cols as usize;
        if out.len() < need {
            return Err(GhosttyError::BadBuffer {
                need,
                got: out.len(),
            });
        }
        let mut raw = vec![sys::KmuxCell::default(); need];
        let mut cursor = sys::KmuxCursor::default();
        let mut modes = sys::KmuxModes::default();
        let rc = unsafe {
            sys::kmux_ghostty_fill_cells_and_cursor(
                self.handle.as_ptr(),
                raw.as_mut_ptr(),
                need,
                &mut cursor,
                &mut modes,
            )
        };
        map_rc(rc)?;
        for (dst, src) in out.iter_mut().zip(raw.iter()).take(need) {
            *dst = convert_cell(src);
        }
        Ok((convert_cursor(&cursor), TermModes(modes.bits)))
    }

    /// Current cursor position and shape.
    #[must_use]
    pub fn cursor(&self) -> CursorState {
        let mut c = sys::KmuxCursor::default();
        unsafe { sys::kmux_ghostty_cursor(self.handle.as_ptr(), &mut c) };
        convert_cursor(&c)
    }

    /// Current mode flags (app-cursor, bracketed-paste, mouse-*).
    #[must_use]
    pub fn modes(&self) -> TermModes {
        let mut m = sys::KmuxModes::default();
        unsafe { sys::kmux_ghostty_modes(self.handle.as_ptr(), &mut m) };
        TermModes(m.bits)
    }

    /// True iff the alternate screen is currently active.
    #[must_use]
    pub fn is_alt_screen(&self) -> bool {
        unsafe { sys::kmux_ghostty_is_alt_screen(self.handle.as_ptr()) }
    }

    /// Number of rows currently retained in scrollback (not counting viewport).
    #[must_use]
    pub fn history_size(&self) -> usize {
        unsafe { sys::kmux_ghostty_history_size(self.handle.as_ptr()) }
    }

    /// Return the current window title set via OSC 0/2, or `None` if no title
    /// has been received yet. Uses a 1 KiB stack buffer — sufficient for any
    /// real terminal title.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        let mut buf = [0u8; 1024];
        let n = unsafe {
            sys::kmux_ghostty_get_title(self.handle.as_ptr(), buf.as_mut_ptr(), buf.len())
        };
        if n == 0 {
            None
        } else {
            String::from_utf8(buf[..n].to_vec()).ok()
        }
    }

    /// Return the latest OSC 9;4 progress report. Defaults to
    /// [`ProgressState::Remove`] (no bar) until the inner program emits a
    /// progress sequence. Pull-based companion to [`EventSink::on_progress`],
    /// used by kmuxd to recover state if a report fired before a subscriber
    /// attached.
    #[must_use]
    pub fn progress(&self) -> ProgressReport {
        let mut state = 0u8;
        let mut value = 0u8;
        let mut has = 0u8;
        unsafe {
            sys::kmux_ghostty_get_progress(self.handle.as_ptr(), &mut state, &mut value, &mut has)
        };
        ProgressReport::from_raw(state, value, has)
    }

    /// Read `count` rows of scrollback starting at `start` (0 = oldest).
    /// Returns one `Vec<CellState>` per row; `cols` is the row width.
    pub fn read_history(
        &self,
        start: usize,
        count: usize,
        cols: usize,
    ) -> Result<Vec<Vec<CellState>>, GhosttyError> {
        if count == 0 || cols == 0 {
            return Ok(Vec::new());
        }
        let need = count * cols;
        let mut raw = vec![sys::KmuxCell::default(); need];
        let mut filled: usize = 0;
        let rc = unsafe {
            sys::kmux_ghostty_read_history(
                self.handle.as_ptr(),
                start,
                count,
                cols,
                raw.as_mut_ptr(),
                need,
                &mut filled,
            )
        };
        map_rc(rc)?;
        let mut out = Vec::with_capacity(filled);
        for r in 0..filled {
            let base = r * cols;
            out.push(raw[base..base + cols].iter().map(convert_cell).collect());
        }
        Ok(out)
    }
}

impl Drop for GhosttyTerm {
    fn drop(&mut self) {
        // Zig frees the Terminal + Stream first; only then is the Box<EventBridge>
        // released as `self._bridge` goes out of scope.
        unsafe { sys::kmux_ghostty_free(self.handle.as_ptr()) };
    }
}

fn convert_cell(c: &sys::KmuxCell) -> CellState {
    CellState {
        c: char::from_u32(c.codepoint).unwrap_or(' '),
        fg: rgba_to_color(c.fg_rgba),
        bg: rgba_to_color(c.bg_rgba),
        attrs: kmux_protocol::messages::CellAttrs(c.attrs),
    }
}

fn rgba_to_color(v: u32) -> CellColor {
    CellColor {
        r: ((v >> 16) & 0xff) as u8,
        g: ((v >> 8) & 0xff) as u8,
        b: (v & 0xff) as u8,
    }
}

fn convert_cursor(c: &sys::KmuxCursor) -> CursorState {
    let shape = match c.shape {
        sys::SHAPE_BLOCK => CursorShape::Block,
        sys::SHAPE_UNDERLINE => CursorShape::Underline,
        sys::SHAPE_BAR => CursorShape::Bar,
        sys::SHAPE_HOLLOW_BLOCK => CursorShape::HollowBlock,
        sys::SHAPE_HIDDEN => CursorShape::Hidden,
        other => panic!("libkmux_ghostty returned unknown cursor shape ordinal {other}"),
    };
    CursorState {
        row: c.row,
        col: c.col,
        shape,
        visible: c.visible != 0,
        blink: c.blink != 0,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Key encoding
// ─────────────────────────────────────────────────────────────────────────────

/// Stable kmux key ordinal. The numeric value of each variant **must** stay
/// in sync with `KmuxKey` in `crates/kmux-ghostty-sys/zig/src/wrapper.zig`.
/// `key_ordinal_drift_check` (in `tests`) pins a few canonical values so a
/// silent reorder breaks the build.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)] // mirrors physical key codes (key_a, digit_0, …)
pub enum Key {
    Unidentified = 0,
    A = 1,
    B = 2,
    C = 3,
    D = 4,
    E = 5,
    F = 6,
    G = 7,
    H = 8,
    I = 9,
    J = 10,
    K = 11,
    L = 12,
    M = 13,
    N = 14,
    O = 15,
    P = 16,
    Q = 17,
    R = 18,
    S = 19,
    T = 20,
    U = 21,
    V = 22,
    W = 23,
    X = 24,
    Y = 25,
    Z = 26,
    Digit0 = 27,
    Digit1 = 28,
    Digit2 = 29,
    Digit3 = 30,
    Digit4 = 31,
    Digit5 = 32,
    Digit6 = 33,
    Digit7 = 34,
    Digit8 = 35,
    Digit9 = 36,
    Backquote = 37,
    Backslash = 38,
    BracketLeft = 39,
    BracketRight = 40,
    Comma = 41,
    Equal = 42,
    Minus = 43,
    Period = 44,
    Quote = 45,
    Semicolon = 46,
    Slash = 47,
    Enter = 48,
    Tab = 49,
    Space = 50,
    Backspace = 51,
    Escape = 52,
    Insert = 53,
    Delete = 54,
    Home = 55,
    End = 56,
    PageUp = 57,
    PageDown = 58,
    ArrowUp = 59,
    ArrowDown = 60,
    ArrowLeft = 61,
    ArrowRight = 62,
    F1 = 63,
    F2 = 64,
    F3 = 65,
    F4 = 66,
    F5 = 67,
    F6 = 68,
    F7 = 69,
    F8 = 70,
    F9 = 71,
    F10 = 72,
    F11 = 73,
    F12 = 74,
    ShiftLeft = 75,
    ShiftRight = 76,
    ControlLeft = 77,
    ControlRight = 78,
    AltLeft = 79,
    AltRight = 80,
    MetaLeft = 81,
    MetaRight = 82,
    CapsLock = 83,
}

impl From<kmux_protocol::messages::KeyCode> for Key {
    fn from(code: kmux_protocol::messages::KeyCode) -> Self {
        use kmux_protocol::messages::KeyCode as P;
        match code {
            P::Unidentified => Key::Unidentified,
            P::A => Key::A,
            P::B => Key::B,
            P::C => Key::C,
            P::D => Key::D,
            P::E => Key::E,
            P::F => Key::F,
            P::G => Key::G,
            P::H => Key::H,
            P::I => Key::I,
            P::J => Key::J,
            P::K => Key::K,
            P::L => Key::L,
            P::M => Key::M,
            P::N => Key::N,
            P::O => Key::O,
            P::P => Key::P,
            P::Q => Key::Q,
            P::R => Key::R,
            P::S => Key::S,
            P::T => Key::T,
            P::U => Key::U,
            P::V => Key::V,
            P::W => Key::W,
            P::X => Key::X,
            P::Y => Key::Y,
            P::Z => Key::Z,
            P::Digit0 => Key::Digit0,
            P::Digit1 => Key::Digit1,
            P::Digit2 => Key::Digit2,
            P::Digit3 => Key::Digit3,
            P::Digit4 => Key::Digit4,
            P::Digit5 => Key::Digit5,
            P::Digit6 => Key::Digit6,
            P::Digit7 => Key::Digit7,
            P::Digit8 => Key::Digit8,
            P::Digit9 => Key::Digit9,
            P::Backquote => Key::Backquote,
            P::Backslash => Key::Backslash,
            P::BracketLeft => Key::BracketLeft,
            P::BracketRight => Key::BracketRight,
            P::Comma => Key::Comma,
            P::Equal => Key::Equal,
            P::Minus => Key::Minus,
            P::Period => Key::Period,
            P::Quote => Key::Quote,
            P::Semicolon => Key::Semicolon,
            P::Slash => Key::Slash,
            P::Enter => Key::Enter,
            P::Tab => Key::Tab,
            P::Space => Key::Space,
            P::Backspace => Key::Backspace,
            P::Escape => Key::Escape,
            P::Insert => Key::Insert,
            P::Delete => Key::Delete,
            P::Home => Key::Home,
            P::End => Key::End,
            P::PageUp => Key::PageUp,
            P::PageDown => Key::PageDown,
            P::ArrowUp => Key::ArrowUp,
            P::ArrowDown => Key::ArrowDown,
            P::ArrowLeft => Key::ArrowLeft,
            P::ArrowRight => Key::ArrowRight,
            P::F1 => Key::F1,
            P::F2 => Key::F2,
            P::F3 => Key::F3,
            P::F4 => Key::F4,
            P::F5 => Key::F5,
            P::F6 => Key::F6,
            P::F7 => Key::F7,
            P::F8 => Key::F8,
            P::F9 => Key::F9,
            P::F10 => Key::F10,
            P::F11 => Key::F11,
            P::F12 => Key::F12,
            P::ShiftLeft => Key::ShiftLeft,
            P::ShiftRight => Key::ShiftRight,
            P::ControlLeft => Key::ControlLeft,
            P::ControlRight => Key::ControlRight,
            P::AltLeft => Key::AltLeft,
            P::AltRight => Key::AltRight,
            P::MetaLeft => Key::MetaLeft,
            P::MetaRight => Key::MetaRight,
            P::CapsLock => Key::CapsLock,
        }
    }
}

bitflags::bitflags! {
    /// Modifier bitmask. Layout matches `gvt.input.KeyMods` (the low byte).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct KeyMods: u16 {
        const SHIFT = sys::KEY_MOD_SHIFT;
        const CTRL  = sys::KEY_MOD_CTRL;
        const ALT   = sys::KEY_MOD_ALT;
        const SUPER = sys::KEY_MOD_SUPER;
    }
}

/// Press / Repeat / Release. Matches `gvt.input.KeyAction`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Release = 0,
    Press = 1,
    Repeat = 2,
}

/// Encoder configuration mirroring `gvt.input.KeyEncodeOptions`.  Read live
/// from a `GhosttyTerm` via [`GhosttyTerm::encoder_options`] to ensure
/// encoding matches what the inner program negotiated.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyEncodeOptions {
    /// DECCKM (DEC mode 1).
    pub cursor_key_application: bool,
    /// DECKPAM (DEC mode 66).
    pub keypad_key_application: bool,
    /// DEC mode 1035.
    pub ignore_keypad_with_numlock: bool,
    /// DEC mode 1036.
    pub alt_esc_prefix: bool,
    /// xterm modifyOtherKeys=2.
    pub modify_other_keys_state_2: bool,
    /// Kitty keyboard protocol flag bitmask. See `sys::KITTY_KBD_*`.
    pub kitty_flags: u8,
}

/// One key event ready to be encoded. Values that are unknown (e.g. the
/// `utf8` text or `unshifted_codepoint`) can be left empty / zero — the
/// encoder falls back gracefully.
#[derive(Debug, Clone, Default)]
pub struct KeyEvent {
    pub key: Option<Key>,
    pub mods: KeyMods,
    pub action: KeyActionDefault,
    pub utf8: String,
    pub unshifted_codepoint: u32,
}

/// New-type so `KeyAction` can be `Default` without losing the explicit
/// reading at call sites. Defaults to `Press`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyActionDefault(pub KeyAction);
impl Default for KeyActionDefault {
    fn default() -> Self {
        Self(KeyAction::Press)
    }
}
impl From<KeyAction> for KeyActionDefault {
    fn from(a: KeyAction) -> Self {
        Self(a)
    }
}

#[derive(Debug, Error)]
pub enum KeyEncodeError {
    #[error("invalid Key or KeyAction ordinal — refusing to encode (likely an ABI drift)")]
    InvalidEnum,
}

/// Encode a single key event into terminal escape bytes.
///
/// The encoder is stateless — the live mode state lives inside the
/// [`GhosttyTerm`] and is queried per call via [`GhosttyTerm::encoder_options`].
/// `Unidentified` keys with empty `utf8` produce no output.
pub fn encode_key(opts: &KeyEncodeOptions, event: &KeyEvent) -> Result<Vec<u8>, KeyEncodeError> {
    let raw_opts = sys::KmuxKeyEncodeOptions {
        cursor_key_application: opts.cursor_key_application as u8,
        keypad_key_application: opts.keypad_key_application as u8,
        ignore_keypad_with_numlock: opts.ignore_keypad_with_numlock as u8,
        alt_esc_prefix: opts.alt_esc_prefix as u8,
        modify_other_keys_state_2: opts.modify_other_keys_state_2 as u8,
        kitty_flags: opts.kitty_flags,
        _pad: [0, 0],
    };
    let key_ord = event.key.unwrap_or(Key::Unidentified) as u16;
    let action_ord = event.action.0 as u8;
    let mods = event.mods.bits();
    let utf8 = event.utf8.as_bytes();
    let utf8_ptr = if utf8.is_empty() {
        core::ptr::null()
    } else {
        utf8.as_ptr()
    };

    // Most encoded sequences are < 32 bytes. Start with a stack-friendly
    // 64-byte heap allocation; the encoder will tell us if we need more.
    let mut out = vec![0u8; 64];
    let mut written: usize = 0;
    let rc = unsafe {
        sys::kmux_ghostty_encode_key(
            &raw_opts,
            key_ord,
            mods,
            action_ord,
            utf8_ptr,
            utf8.len(),
            event.unshifted_codepoint,
            out.as_mut_ptr(),
            out.len(),
            &mut written,
        )
    };
    match rc {
        sys::ENC_OK => {
            out.truncate(written);
            Ok(out)
        }
        sys::ENC_OUT_OF_MEMORY => {
            // `written` now holds the required buffer size.
            out.resize(written, 0);
            let mut written2: usize = 0;
            let rc2 = unsafe {
                sys::kmux_ghostty_encode_key(
                    &raw_opts,
                    key_ord,
                    mods,
                    action_ord,
                    utf8_ptr,
                    utf8.len(),
                    event.unshifted_codepoint,
                    out.as_mut_ptr(),
                    out.len(),
                    &mut written2,
                )
            };
            assert_eq!(
                rc2,
                sys::ENC_OK,
                "second encode_key call should fit after resizing buffer to required size"
            );
            out.truncate(written2);
            Ok(out)
        }
        sys::ENC_INVALID_ENUM => Err(KeyEncodeError::InvalidEnum),
        other => panic!("kmux_ghostty_encode_key returned unexpected code {other}"),
    }
}

impl GhosttyTerm {
    /// Read the current key-encoder options from the terminal's live mode
    /// state (DECCKM, DECKPAM, modifyOtherKeys, kitty kbd flags).  Pass the
    /// returned struct to [`encode_key`] so encoding always matches what the
    /// inner program negotiated.
    #[must_use]
    pub fn encoder_options(&self) -> KeyEncodeOptions {
        let modes = self.modes();
        let kitty = unsafe { sys::kmux_ghostty_kitty_flags(self.handle.as_ptr()) };
        KeyEncodeOptions {
            cursor_key_application: modes.app_cursor(),
            // The protocol-level TermModes only carries app_cursor, so we
            // approximate the rest from the kitty_flags + xterm extensions.
            // DECKPAM, modifyOtherKeys, alt_esc_prefix, and
            // ignore_keypad_with_numlock are not currently mirrored on the
            // wire — encoding still works because the encoder falls back to
            // sensible defaults for them.
            keypad_key_application: false,
            ignore_keypad_with_numlock: false,
            alt_esc_prefix: false,
            modify_other_keys_state_2: false,
            kitty_flags: kitty,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn size(rows: u16, cols: u16) -> TermSize {
        TermSize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[test]
    fn abi_version_matches_build_time_constant() {
        assert_eq!(abi_version(), EXPECTED_ABI_VERSION);
        check_abi_version();
    }

    #[test]
    fn roundtrip_feed_fill_cells_hello() {
        let mut term = GhosttyTerm::new(size(4, 20), 1000, Arc::new(NullSink)).unwrap();
        term.feed(b"hello").unwrap();
        let mut out = vec![CellState::default(); 4 * 20];
        term.fill_cells(&mut out).unwrap();
        let s: String = out[..5].iter().map(|c| c.c).collect();
        assert_eq!(s, "hello");
    }

    #[test]
    fn cursor_reports_decscusr_blink_request() {
        let mut term = GhosttyTerm::new(size(4, 20), 1000, Arc::new(NullSink)).unwrap();
        // Blinking bar (DECSCUSR 5): bar shape, blink requested.
        term.feed(b"\x1b[5 q").unwrap();
        let c = term.cursor();
        assert_eq!(c.shape, CursorShape::Bar);
        assert!(c.blink, "DECSCUSR 5 should request a blinking cursor");
        // Steady bar (DECSCUSR 6): bar shape, no blink.
        term.feed(b"\x1b[6 q").unwrap();
        let c = term.cursor();
        assert_eq!(c.shape, CursorShape::Bar);
        assert!(!c.blink, "DECSCUSR 6 should request a steady cursor");
    }

    #[test]
    fn default_cursor_blinks() {
        let mut term = GhosttyTerm::new(size(4, 20), 1000, Arc::new(NullSink)).unwrap();
        // A program that never issues DECSCUSR must blink, like a real terminal.
        assert!(term.cursor().blink, "the default cursor should blink");
        // DECSCUSR 0 (and the no-param form) is the blinking default per xterm.
        term.feed(b"\x1b[0 q").unwrap();
        assert!(
            term.cursor().blink,
            "DECSCUSR 0 should request a blinking cursor"
        );
        // An explicit steady block (DECSCUSR 2) still clears blink.
        term.feed(b"\x1b[2 q").unwrap();
        assert!(
            !term.cursor().blink,
            "DECSCUSR 2 should request a steady cursor"
        );
    }

    #[test]
    fn send_across_thread_feed_and_drop() {
        // Exercises the `unsafe impl Send` — moves the handle to another
        // thread, feeds bytes, joins. Catches any TLS/thread-local assumption
        // that would otherwise only surface at runtime.
        let term = GhosttyTerm::new(size(3, 10), 100, Arc::new(NullSink)).unwrap();
        let handle = std::thread::spawn(move || {
            let mut t = term;
            t.feed(b"hi").unwrap();
            let mut cells = vec![CellState::default(); 30];
            t.fill_cells(&mut cells).unwrap();
            (cells[0].c, cells[1].c)
        });
        assert_eq!(handle.join().unwrap(), ('h', 'i'));
    }

    #[derive(Default)]
    struct TitleRecorder(Mutex<Vec<String>>);

    impl EventSink for TitleRecorder {
        fn on_title(&self, title: &str) {
            self.0.lock().unwrap().push(title.to_owned());
        }
    }

    #[test]
    fn event_sink_title_trampoline() {
        let rec = Arc::new(TitleRecorder::default());
        let mut term =
            GhosttyTerm::new(size(4, 20), 100, rec.clone() as Arc<dyn EventSink>).unwrap();
        // OSC 0 ; hi BEL — sets window title.
        term.feed(b"\x1b]0;hello-world\x07").unwrap();
        let titles = rec.0.lock().unwrap().clone();
        assert_eq!(titles, vec!["hello-world".to_owned()]);
    }

    #[test]
    fn title_getter_returns_none_before_osc_and_value_after() {
        let mut term = GhosttyTerm::new(size(4, 20), 100, Arc::new(NullSink)).unwrap();
        assert_eq!(term.title(), None);
        term.feed(b"\x1b]0;My Title\x07").unwrap();
        assert_eq!(term.title().as_deref(), Some("My Title"));
        // OSC 2 also sets the title.
        term.feed(b"\x1b]2;Updated\x07").unwrap();
        assert_eq!(term.title().as_deref(), Some("Updated"));
    }

    #[derive(Default)]
    struct ProgressRecorder(Mutex<Vec<ProgressReport>>);
    impl EventSink for ProgressRecorder {
        fn on_progress(&self, report: ProgressReport) {
            self.0.lock().unwrap().push(report);
        }
    }

    #[test]
    fn event_sink_progress_trampoline() {
        let rec = Arc::new(ProgressRecorder::default());
        let mut term =
            GhosttyTerm::new(size(4, 20), 100, rec.clone() as Arc<dyn EventSink>).unwrap();
        // OSC 9;4;2;30 — error state at 30%.
        term.feed(b"\x1b]9;4;2;30\x07").unwrap();
        let got = rec.0.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![ProgressReport {
                state: ProgressState::Error,
                progress: Some(30),
            }]
        );
    }

    #[test]
    fn progress_getter_tracks_latest_state() {
        let mut term = GhosttyTerm::new(size(4, 20), 100, Arc::new(NullSink)).unwrap();
        assert_eq!(term.progress(), ProgressReport::none());
        // Set, 50%.
        term.feed(b"\x1b]9;4;1;50\x07").unwrap();
        assert_eq!(
            term.progress(),
            ProgressReport {
                state: ProgressState::Set,
                progress: Some(50),
            }
        );
        // Indeterminate carries no value.
        term.feed(b"\x1b]9;4;3\x07").unwrap();
        assert_eq!(
            term.progress(),
            ProgressReport {
                state: ProgressState::Indeterminate,
                progress: None,
            }
        );
        // Remove clears the bar.
        term.feed(b"\x1b]9;4;0\x07").unwrap();
        assert_eq!(term.progress(), ProgressReport::none());
    }

    #[derive(Default)]
    struct DropCounter(Arc<AtomicUsize>);
    impl EventSink for DropCounter {
        fn on_title(&self, _: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1000, Ordering::SeqCst);
        }
    }

    #[test]
    fn drop_order_free_terminal_then_release_arc() {
        // Precondition: the `Arc<dyn EventSink>` must be held via the
        // `EventBridge` until `GhosttyTerm` is dropped. Here we take a weak
        // reference, drop the terminal, and verify the sink is released
        // (strong-count observable via `Arc::strong_count`).
        let counter = Arc::new(AtomicUsize::new(0));
        let sink: Arc<DropCounter> = Arc::new(DropCounter(counter.clone()));
        let weak = Arc::downgrade(&sink);
        let term = GhosttyTerm::new(size(3, 3), 10, sink as Arc<dyn EventSink>).unwrap();
        // Terminal is alive: sink has two strong refs (ours just went; bridge has one).
        assert_eq!(weak.strong_count(), 1);
        drop(term);
        // After drop, the bridge is gone and the sink was released.
        assert_eq!(weak.strong_count(), 0);
        // The Drop impl bumped counter by 1000 exactly once.
        assert_eq!(counter.load(Ordering::SeqCst), 1000);
    }

    #[test]
    fn resize_roundtrip_preserves_dimensions() {
        let mut term = GhosttyTerm::new(size(10, 30), 100, Arc::new(NullSink)).unwrap();
        assert_eq!(term.size(), size(10, 30));
        term.resize(size(20, 60)).unwrap();
        assert_eq!(term.size(), size(20, 60));
    }

    #[test]
    fn invalid_size_rejected() {
        let err = GhosttyTerm::new(size(0, 10), 10, Arc::new(NullSink)).unwrap_err();
        assert!(matches!(err, GhosttyError::InvalidSize));
    }

    // ── Key encoder ────────────────────────────────────────────────────

    /// Pin canonical Key ordinals.  If the Zig `KmuxKey` enum is reordered
    /// without updating Rust's mirror (or vice versa), this test fails
    /// loudly instead of silently encoding the wrong key on the wire.
    #[test]
    fn key_ordinals_match_zig() {
        assert_eq!(Key::Unidentified as u16, 0);
        assert_eq!(Key::A as u16, 1);
        assert_eq!(Key::Z as u16, 26);
        assert_eq!(Key::Digit0 as u16, 27);
        assert_eq!(Key::Enter as u16, 48);
        assert_eq!(Key::Tab as u16, 49);
        assert_eq!(Key::Backspace as u16, 51);
        assert_eq!(Key::Escape as u16, 52);
        assert_eq!(Key::ArrowUp as u16, 59);
        assert_eq!(Key::F1 as u16, 63);
        assert_eq!(Key::CapsLock as u16, 83);
    }

    fn enc_default() -> KeyEncodeOptions {
        KeyEncodeOptions::default()
    }

    fn enc_kitty() -> KeyEncodeOptions {
        KeyEncodeOptions {
            kitty_flags: sys::KITTY_KBD_DISAMBIGUATE,
            ..Default::default()
        }
    }

    #[test]
    fn encode_plain_enter_is_cr() {
        let bytes = encode_key(
            &enc_default(),
            &KeyEvent {
                key: Some(Key::Enter),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\r");
    }

    #[test]
    fn encode_plain_tab_is_tab() {
        let bytes = encode_key(
            &enc_default(),
            &KeyEvent {
                key: Some(Key::Tab),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\t");
    }

    #[test]
    fn encode_shift_tab_is_csi_z() {
        // Shift+Tab → xterm CBT, regardless of kitty flags.
        let bytes = encode_key(
            &enc_default(),
            &KeyEvent {
                key: Some(Key::Tab),
                mods: KeyMods::SHIFT,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[Z");
    }

    #[test]
    fn encode_shift_enter_with_kitty_is_csi_u() {
        // Shift+Enter with kitty disambiguate → CSI 13;2u.
        // This is what Claude Code expects after `\x1b[>1u` negotiation.
        let bytes = encode_key(
            &enc_kitty(),
            &KeyEvent {
                key: Some(Key::Enter),
                mods: KeyMods::SHIFT,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[13;2u");
    }

    #[test]
    fn encode_alt_enter_with_kitty_is_csi_u_mod3() {
        let bytes = encode_key(
            &enc_kitty(),
            &KeyEvent {
                key: Some(Key::Enter),
                mods: KeyMods::ALT,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[13;3u");
    }

    #[test]
    fn encode_ctrl_arrow_up_modifies_csi_a() {
        // Ctrl+Up → ESC[1;5A in xterm legacy.
        let bytes = encode_key(
            &enc_default(),
            &KeyEvent {
                key: Some(Key::ArrowUp),
                mods: KeyMods::CTRL,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, b"\x1b[1;5A");
    }

    #[test]
    fn encode_unidentified_with_no_text_is_empty() {
        let bytes = encode_key(&enc_default(), &KeyEvent::default()).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn encoder_options_reads_live_kitty_flags() {
        let mut term = GhosttyTerm::new(size(4, 20), 100, Arc::new(NullSink)).unwrap();
        let opts0 = term.encoder_options();
        assert_eq!(opts0.kitty_flags, 0, "no flags before negotiation");

        // Simulate the inner program enabling kitty kbd disambiguate.
        term.feed(b"\x1b[>1u").unwrap();

        let opts1 = term.encoder_options();
        assert!(
            opts1.kitty_flags & sys::KITTY_KBD_DISAMBIGUATE != 0,
            "disambiguate bit must be set after \\x1b[>1u, got {:#04x}",
            opts1.kitty_flags
        );
    }

    #[test]
    fn encoder_options_reads_live_app_cursor() {
        let mut term = GhosttyTerm::new(size(4, 20), 100, Arc::new(NullSink)).unwrap();
        assert!(!term.encoder_options().cursor_key_application);
        term.feed(b"\x1b[?1h").unwrap(); // DECCKM on
        assert!(term.encoder_options().cursor_key_application);
        term.feed(b"\x1b[?1l").unwrap(); // DECCKM off
        assert!(!term.encoder_options().cursor_key_application);
    }
}
