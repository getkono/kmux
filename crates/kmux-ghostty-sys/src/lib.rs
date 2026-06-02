//! Raw FFI bindings to `libkmux_ghostty` — a Zig static library that wraps
//! libghostty-vt and exports a kmux-owned, stable C ABI.
//!
//! This crate is intentionally thin: only `#[repr(C)]` types and `extern "C"`
//! declarations live here. The safe Rust surface is in `kmux-ghostty`.
//!
//! FFI invariants enforced by every binding in this module:
//! - No ownership transfer across the boundary in either direction.
//! - All pointer parameters are borrowed; valid only for the duration of the
//!   individual call (or, for event-sink callbacks, only for the callback).
//! - Output buffers are caller-allocated; the Zig side never allocates memory
//!   that Rust is expected to free.
//! - `kmux_ghostty_term` is opaque; construct it via `kmux_ghostty_new` and
//!   destroy it with `kmux_ghostty_free`.

#![deny(missing_debug_implementations)]

use core::ffi::c_void;

/// ABI version expected by this Rust crate. The Zig wrapper exports the same
/// constant via [`kmux_ghostty_abi_version`]. Mismatch is a build-time
/// inconsistency — safe wrappers must panic on mismatch.
pub const EXPECTED_ABI_VERSION: u32 = 3;

// Result codes returned by the Zig wrapper. `OK` is 0; everything else is
// a negative error code. Kept in sync with `src/wrapper.zig`.
pub const KMUX_OK: i32 = 0;
pub const KMUX_ERR_ALLOC: i32 = -1;
pub const KMUX_ERR_INVALID_SIZE: i32 = -2;
pub const KMUX_ERR_FEED: i32 = -3;
pub const KMUX_ERR_RESIZE: i32 = -4;
pub const KMUX_ERR_BAD_BUFFER: i32 = -5;

// Key encoder result codes (separate namespace; introduced in ABI v2).
pub const ENC_OK: i32 = 0;
pub const ENC_OUT_OF_MEMORY: i32 = -10;
pub const ENC_INVALID_ENUM: i32 = -11;

// Kitty keyboard protocol flag bits (matches Ghostty's `KittyFlags` packed
// struct).  Returned by `kmux_ghostty_kitty_flags` and accepted by the
// `kitty_flags` field of `KmuxKeyEncodeOptions`.
pub const KITTY_KBD_DISAMBIGUATE: u8 = 1 << 0;
pub const KITTY_KBD_REPORT_EVENTS: u8 = 1 << 1;
pub const KITTY_KBD_REPORT_ALTERNATES: u8 = 1 << 2;
pub const KITTY_KBD_REPORT_ALL: u8 = 1 << 3;
pub const KITTY_KBD_REPORT_ASSOCIATED: u8 = 1 << 4;

// Key event modifier bits (matches Ghostty's `Mods` packed struct, low byte).
pub const KEY_MOD_SHIFT: u16 = 1 << 0;
pub const KEY_MOD_CTRL: u16 = 1 << 1;
pub const KEY_MOD_ALT: u16 = 1 << 2;
pub const KEY_MOD_SUPER: u16 = 1 << 3;

// Key action ordinals (matches Ghostty's `Action` enum(c_int)).
pub const KEY_ACTION_RELEASE: u8 = 0;
pub const KEY_ACTION_PRESS: u8 = 1;
pub const KEY_ACTION_REPEAT: u8 = 2;

// Cell attr bits (see `CellAttrs` in kmux-protocol; lock-stepped with
// wrapper.zig's `ATTR_*` constants).
pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_ITALIC: u16 = 1 << 1;
pub const ATTR_UNDERLINE: u16 = 1 << 2;
pub const ATTR_STRIKETHROUGH: u16 = 1 << 3;
pub const ATTR_INVERSE: u16 = 1 << 4;
pub const ATTR_HIDDEN: u16 = 1 << 5;
pub const ATTR_DIM: u16 = 1 << 6;
pub const ATTR_BLINK: u16 = 1 << 7;
pub const ATTR_WIDE_CHAR: u16 = 1 << 8;
pub const ATTR_WIDE_CHAR_SPACER: u16 = 1 << 9;
pub const ATTR_DEFAULT_FG: u16 = 1 << 10;
pub const ATTR_DEFAULT_BG: u16 = 1 << 11;

// Term mode bits (see `TermModes` in kmux-protocol).
pub const MODE_APP_CURSOR: u16 = 1 << 0;
pub const MODE_BRACKETED_PASTE: u16 = 1 << 1;
pub const MODE_MOUSE_REPORT_CLICK: u16 = 1 << 2;
pub const MODE_MOUSE_DRAG: u16 = 1 << 3;
pub const MODE_MOUSE_MOTION: u16 = 1 << 4;
pub const MODE_SGR_MOUSE: u16 = 1 << 5;

// Cursor shape ordinals (see `CursorShape` in kmux-protocol).
pub const SHAPE_BLOCK: u8 = 0;
pub const SHAPE_UNDERLINE: u8 = 1;
pub const SHAPE_BAR: u8 = 2;
pub const SHAPE_HOLLOW_BLOCK: u8 = 3;
pub const SHAPE_HIDDEN: u8 = 4;

/// Opaque terminal handle. Layout is private to the Zig wrapper.
#[repr(C)]
#[derive(Debug)]
pub struct kmux_ghostty_term {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KmuxSize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KmuxCell {
    pub codepoint: u32,
    pub fg_rgba: u32,
    pub bg_rgba: u32,
    pub attrs: u16,
    pub width: u8,
    pub _pad: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KmuxCursor {
    pub row: u16,
    pub col: u16,
    pub shape: u8,
    pub visible: u8,
    /// 1 if the inner program requested a blinking cursor (DECSCUSR
    /// `blinking_*` / DEC mode 12 `cursor_blinking`), 0 for steady.
    pub blink: u8,
    pub _pad: [u8; 1],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KmuxModes {
    pub bits: u16,
}

/// Options forwarded to `gvt.input.encodeKey` for a single key event.
/// Fields mirror Ghostty's `KeyEncodeOptions`.  All booleans are `u8`
/// (0 = false, non-zero = true) so the layout is stable across Rust/Zig.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct KmuxKeyEncodeOptions {
    pub cursor_key_application: u8,
    pub keypad_key_application: u8,
    pub ignore_keypad_with_numlock: u8,
    pub alt_esc_prefix: u8,
    pub modify_other_keys_state_2: u8,
    pub kitty_flags: u8,
    pub _pad: [u8; 2],
}

/// Event-sink vtable. All callbacks fire synchronously inside `kmux_ghostty_feed`.
/// Callers **must not** retain any of the pointer arguments past callback return.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KmuxEventSink {
    pub user: *mut c_void,
    pub on_title: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize)>,
    pub on_bell: Option<unsafe extern "C" fn(*mut c_void)>,
    pub on_osc52: Option<unsafe extern "C" fn(*mut c_void, u8, *const u8, usize)>,
    pub on_hyperlink: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize)>,
}

// Safety: the sink is an inert vtable of function pointers plus an opaque
// `user` pointer whose thread-safety is the caller's concern. The struct
// itself carries no interior mutability.
unsafe impl Send for KmuxEventSink {}
unsafe impl Sync for KmuxEventSink {}

impl Default for KmuxEventSink {
    fn default() -> Self {
        Self {
            user: core::ptr::null_mut(),
            on_title: None,
            on_bell: None,
            on_osc52: None,
            on_hyperlink: None,
        }
    }
}

unsafe extern "C" {
    /// ABI version baked into `libkmux_ghostty.a` at build time. Safe wrappers
    /// panic on mismatch with [`EXPECTED_ABI_VERSION`].
    pub fn kmux_ghostty_abi_version() -> u32;

    /// Construct a new terminal. On success writes the handle into `*out` and
    /// returns [`KMUX_OK`].
    pub fn kmux_ghostty_new(
        size: *const KmuxSize,
        scrollback: u32,
        sink: *const KmuxEventSink,
        out: *mut *mut kmux_ghostty_term,
    ) -> i32;

    /// Destroy a terminal handle. Accepts NULL (no-op).
    pub fn kmux_ghostty_free(term: *mut kmux_ghostty_term);

    /// Feed VT bytes. Callbacks (bell/title/OSC52/OSC8) fire synchronously.
    pub fn kmux_ghostty_feed(term: *mut kmux_ghostty_term, ptr: *const u8, len: usize) -> i32;

    /// Resize the grid (preserves scrollback). Returns [`KMUX_ERR_INVALID_SIZE`]
    /// on zero dimensions.
    pub fn kmux_ghostty_resize(term: *mut kmux_ghostty_term, size: *const KmuxSize) -> i32;

    /// Read current grid dimensions + pixel-size hint.
    pub fn kmux_ghostty_size(term: *const kmux_ghostty_term, out: *mut KmuxSize);

    /// Populate `cells[0..rows*cols]` with the active-screen grid in row-major
    /// order. Cells outside the grid are untouched; blanks in the grid are
    /// normalised to a single space. Returns [`KMUX_ERR_BAD_BUFFER`] if
    /// `cells_len < rows*cols`.
    pub fn kmux_ghostty_fill_cells(
        term: *const kmux_ghostty_term,
        cells: *mut KmuxCell,
        cells_len: usize,
    ) -> i32;

    /// Combined active-grid + cursor + modes read in one FFI crossing — used on
    /// the hot path in `DiffEngine` to keep kmuxd's per-frame overhead flat.
    pub fn kmux_ghostty_fill_cells_and_cursor(
        term: *const kmux_ghostty_term,
        cells: *mut KmuxCell,
        cells_len: usize,
        cursor_out: *mut KmuxCursor,
        modes_out: *mut KmuxModes,
    ) -> i32;

    pub fn kmux_ghostty_cursor(term: *const kmux_ghostty_term, out: *mut KmuxCursor);
    pub fn kmux_ghostty_modes(term: *const kmux_ghostty_term, out: *mut KmuxModes);

    /// True iff the alternate screen is currently active.
    pub fn kmux_ghostty_is_alt_screen(term: *const kmux_ghostty_term) -> bool;

    /// Copy the current window title into `out[0..buf_len]`.
    /// Returns bytes written (0 = no title set yet). Does NOT NUL-terminate.
    pub fn kmux_ghostty_get_title(
        term: *const kmux_ghostty_term,
        out: *mut u8,
        buf_len: usize,
    ) -> usize;

    /// Number of rows currently held in scrollback (not counting the viewport).
    pub fn kmux_ghostty_history_size(term: *const kmux_ghostty_term) -> usize;

    /// Read a contiguous window of scrollback rows into the caller's buffer.
    /// `out_rows_filled` reports how many rows were actually written.
    pub fn kmux_ghostty_read_history(
        term: *const kmux_ghostty_term,
        start: usize,
        count: usize,
        cols: usize,
        cells: *mut KmuxCell,
        cells_len: usize,
        out_rows_filled: *mut usize,
    ) -> i32;

    /// Current Kitty keyboard protocol flags as set via `\x1b[>Nu` /
    /// `\x1b[<Nu` push/pop sequences. Returns the packed `KittyFlags` u5
    /// widened to u8 (`KITTY_KBD_*` constants).  Always 0 if the inner
    /// program never enabled the protocol.
    pub fn kmux_ghostty_kitty_flags(term: *const kmux_ghostty_term) -> u8;

    /// Encode a single key event into terminal escape bytes.
    ///
    /// `key`: ordinal of the kmux-stable `KmuxKey` enum (translated to
    ///   `gvt.input.Key` inside the Zig wrapper). See `kmux_ghostty::Key`
    ///   for the safe Rust mirror.
    /// `mods`: packed `KEY_MOD_*` bits.
    /// `action`: `KEY_ACTION_PRESS` / `KEY_ACTION_REPEAT` / `KEY_ACTION_RELEASE`.
    /// `utf8` / `utf8_len`: optional UTF-8 text the keystroke would produce
    ///   in a plain text field. May be NULL/0 for unmapped named keys.
    /// `unshifted_codepoint`: codepoint when no shift is applied (0 = unknown).
    /// `out_buf` / `out_buf_len`: caller's output buffer; may be NULL/0 to
    ///   query the required size — the call returns `ENC_OUT_OF_MEMORY` and
    ///   writes the required size into `out_written`.
    /// `out_written`: bytes written on `ENC_OK`, or required size on
    ///   `ENC_OUT_OF_MEMORY`.
    ///
    /// Returns `ENC_OK`, `ENC_OUT_OF_MEMORY`, or `ENC_INVALID_ENUM` (on
    /// out-of-range `key` or `action` ordinals).
    #[allow(clippy::too_many_arguments)]
    pub fn kmux_ghostty_encode_key(
        opts: *const KmuxKeyEncodeOptions,
        key: u16,
        mods: u16,
        action: u8,
        utf8: *const u8,
        utf8_len: usize,
        unshifted_codepoint: u32,
        out_buf: *mut u8,
        out_buf_len: usize,
        out_written: *mut usize,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_matches_expected() {
        let v = unsafe { kmux_ghostty_abi_version() };
        assert_eq!(
            v, EXPECTED_ABI_VERSION,
            "libkmux_ghostty ABI ({v}) does not match EXPECTED_ABI_VERSION ({EXPECTED_ABI_VERSION})",
        );
    }

    #[test]
    fn struct_layouts_match_wrapper() {
        // Sizes here are the contract the Zig wrapper assumes; changing them
        // requires a coordinated edit on the Zig side.
        assert_eq!(core::mem::size_of::<KmuxSize>(), 8);
        assert_eq!(core::mem::size_of::<KmuxCell>(), 16);
        assert_eq!(core::mem::size_of::<KmuxCursor>(), 8);
        assert_eq!(core::mem::size_of::<KmuxModes>(), 2);
        assert_eq!(core::mem::size_of::<KmuxKeyEncodeOptions>(), 8);
    }

    /// Out-of-range ordinals must not segfault — the Zig wrapper validates
    /// `key` and `action` before any unsafe enum cast.  Per-key encoding
    /// behaviour is exercised in the safe `kmux_ghostty::KeyEncoder` tests
    /// where Rust owns the `gvt.input.Key` ordinal mapping.
    #[test]
    fn encode_invalid_key_returns_invalid_enum() {
        let opts = KmuxKeyEncodeOptions::default();
        let mut out = [0u8; 16];
        let mut written: usize = 0;
        let rc = unsafe {
            kmux_ghostty_encode_key(
                &opts,
                65000, // out of range for KmuxKey
                0,
                KEY_ACTION_PRESS,
                core::ptr::null(),
                0,
                0,
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        assert_eq!(rc, ENC_INVALID_ENUM);
    }

    #[test]
    fn encode_invalid_action_returns_invalid_enum() {
        let opts = KmuxKeyEncodeOptions::default();
        let mut out = [0u8; 16];
        let mut written: usize = 0;
        let rc = unsafe {
            kmux_ghostty_encode_key(
                &opts,
                0,
                0,
                99, // not 0/1/2
                core::ptr::null(),
                0,
                0,
                out.as_mut_ptr(),
                out.len(),
                &mut written,
            )
        };
        assert_eq!(rc, ENC_INVALID_ENUM);
    }

    #[test]
    fn feed_fill_roundtrip_hello() {
        let size = KmuxSize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let sink = KmuxEventSink::default();
        let mut term: *mut kmux_ghostty_term = core::ptr::null_mut();
        let rc = unsafe { kmux_ghostty_new(&size, 1000, &sink, &mut term) };
        assert_eq!(rc, KMUX_OK);
        assert!(!term.is_null());

        let msg = b"hello";
        let rc = unsafe { kmux_ghostty_feed(term, msg.as_ptr(), msg.len()) };
        assert_eq!(rc, KMUX_OK);

        let total = (size.rows as usize) * (size.cols as usize);
        let mut cells = vec![KmuxCell::default(); total];
        let rc = unsafe { kmux_ghostty_fill_cells(term, cells.as_mut_ptr(), cells.len()) };
        assert_eq!(rc, KMUX_OK);

        let got: Vec<u32> = cells[..5].iter().map(|c| c.codepoint).collect();
        assert_eq!(
            got,
            vec![
                b'h' as u32,
                b'e' as u32,
                b'l' as u32,
                b'l' as u32,
                b'o' as u32
            ]
        );

        unsafe { kmux_ghostty_free(term) };
    }
}
