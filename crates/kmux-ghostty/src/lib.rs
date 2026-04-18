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
}
