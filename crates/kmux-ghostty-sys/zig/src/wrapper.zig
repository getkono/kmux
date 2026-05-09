//! kmux wrapper around libghostty-vt v1.3.1. Owns a single `Terminal` per
//! pane and exposes a small, kmux-controlled C ABI that the Rust
//! `kmux-ghostty` crate calls.
//!
//! Design invariants:
//!   * The wrapper is a single heap allocation (`*Wrapper`). The pointer is
//!     opaque to Rust (`*kmux_ghostty_term`).
//!   * Bytes across FFI are borrowed — we never retain caller pointers.
//!   * Output buffers are caller-allocated. We never hand back memory Rust is
//!     expected to free.
//!   * The event-sink callbacks fire synchronously inside `feed` — Rust's
//!     trampoline must not block.
//!
//! ABI version: bump `ABI_VERSION` on any breaking change.

const std = @import("std");
const gvt = @import("ghostty_vt");
const builtin = @import("builtin");

const Terminal = gvt.Terminal;
const Action = gvt.StreamAction;

// -----------------------------------------------------------------------------
// ABI constants
// -----------------------------------------------------------------------------

pub const ABI_VERSION: u32 = 2;

// Result codes. Must match the values `kmux-ghostty-sys::error` expects.
const OK: i32 = 0;
const ERR_ALLOC: i32 = -1;
const ERR_INVALID_SIZE: i32 = -2;
const ERR_FEED: i32 = -3;
const ERR_RESIZE: i32 = -4;
const ERR_BAD_BUFFER: i32 = -5;

// Key encoding result codes. Distinct namespace so the existing wrapper
// errors stay backward-compatible.
const ENC_OK: i32 = 0;
/// Output buffer too small; `out_written` contains the required size.
const ENC_OUT_OF_MEMORY: i32 = -10;
/// `key` ordinal does not map to any `gvt.input.Key` value, or `action`
/// ordinal is not 0/1/2.
const ENC_INVALID_ENUM: i32 = -11;

// Attr bits. Keep in lock-step with `CellAttrs::*` in `kmux-protocol`.
const ATTR_BOLD: u16 = 1 << 0;
const ATTR_ITALIC: u16 = 1 << 1;
const ATTR_UNDERLINE: u16 = 1 << 2;
const ATTR_STRIKETHROUGH: u16 = 1 << 3;
const ATTR_INVERSE: u16 = 1 << 4;
const ATTR_HIDDEN: u16 = 1 << 5;
const ATTR_DIM: u16 = 1 << 6;
const ATTR_BLINK: u16 = 1 << 7;
const ATTR_WIDE_CHAR: u16 = 1 << 8;
const ATTR_WIDE_CHAR_SPACER: u16 = 1 << 9;
const ATTR_DEFAULT_FG: u16 = 1 << 10;
const ATTR_DEFAULT_BG: u16 = 1 << 11;

// TermModes bits. Keep in lock-step with `TermModes::*` in `kmux-protocol`.
const MODE_APP_CURSOR: u16 = 1 << 0;
const MODE_BRACKETED_PASTE: u16 = 1 << 1;
const MODE_MOUSE_REPORT_CLICK: u16 = 1 << 2;
const MODE_MOUSE_DRAG: u16 = 1 << 3;
const MODE_MOUSE_MOTION: u16 = 1 << 4;
const MODE_SGR_MOUSE: u16 = 1 << 5;

// Cursor shape ordinals. Match `CursorShape` in `kmux-protocol::messages::vt`.
const SHAPE_BLOCK: u8 = 0;
const SHAPE_UNDERLINE: u8 = 1;
const SHAPE_BAR: u8 = 2;
const SHAPE_HOLLOW_BLOCK: u8 = 3;
const SHAPE_HIDDEN: u8 = 4;

// Fallback RGB values, matching `CellState::default()` in `kmux-protocol`. Used
// only when the terminal's `DynamicRGB` default is unset; clients substitute
// their own theme via the `DEFAULT_FG`/`DEFAULT_BG` bits regardless.
const FALLBACK_FG: gvt.color.RGB = .{ .r = 0xab, .g = 0xb2, .b = 0xbf };
const FALLBACK_BG: gvt.color.RGB = .{ .r = 0x28, .g = 0x2c, .b = 0x34 };

// -----------------------------------------------------------------------------
// C struct layout (must mirror `kmux-ghostty-sys::ffi`)
// -----------------------------------------------------------------------------

const KmuxSize = extern struct {
    rows: u16,
    cols: u16,
    pixel_width: u16,
    pixel_height: u16,
};

const KmuxCell = extern struct {
    codepoint: u32,
    fg_rgba: u32,
    bg_rgba: u32,
    attrs: u16,
    width: u8,
    _pad: u8,
};

const KmuxCursor = extern struct {
    row: u16,
    col: u16,
    shape: u8,
    visible: u8,
    _pad: [2]u8,
};

const KmuxModes = extern struct {
    bits: u16,
};

/// C-ABI struct mirroring `gvt.input.KeyEncodeOptions`. All booleans are
/// represented as `u8` (0 = false, non-zero = true) so the layout is
/// compiler-stable across Rust/Zig.  `kitty_flags` is the packed bitfield
/// (`KittyFlags` is `packed struct(u5)`); top three bits are ignored.
const KmuxKeyEncodeOptions = extern struct {
    cursor_key_application: u8,
    keypad_key_application: u8,
    ignore_keypad_with_numlock: u8,
    alt_esc_prefix: u8,
    modify_other_keys_state_2: u8,
    kitty_flags: u8,
    _pad: [2]u8,
};

/// Stable kmux-owned key ordinal.  Translates to `gvt.input.Key` via the
/// `kmuxKeyToGvt` switch below; new entries here require a switch entry.
///
/// We do NOT directly expose `gvt.input.Key`'s ordinals over FFI because
/// that enum's order is owned by Ghostty and would silently change kmux's
/// wire ABI any time upstream renumbered.  By owning the ordinals here we
/// can re-pin the mapping with a Zig compile error rather than a crash.
const KmuxKey = enum(u16) {
    unidentified = 0,
    // Letters
    a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, w, x, y, z,
    // Digits
    digit_0, digit_1, digit_2, digit_3, digit_4, digit_5, digit_6, digit_7, digit_8, digit_9,
    // Punctuation that has its own physical key
    backquote, backslash, bracket_left, bracket_right, comma,
    equal, minus, period, quote, semicolon, slash,
    // Functional keys
    enter, tab, space, backspace, escape,
    // Editing keys
    insert, delete, home, end, page_up, page_down,
    // Arrows
    arrow_up, arrow_down, arrow_left, arrow_right,
    // Function keys
    f1, f2, f3, f4, f5, f6, f7, f8, f9, f10, f11, f12,
    // Modifier keys (rarely sent — the Kitty protocol reports them when
    // `report_all` is on; harmless otherwise).
    shift_left, shift_right,
    control_left, control_right,
    alt_left, alt_right,
    meta_left, meta_right,
    caps_lock,
};

fn kmuxKeyToGvt(k: KmuxKey) gvt.input.Key {
    return switch (k) {
        .unidentified => .unidentified,
        .a => .key_a, .b => .key_b, .c => .key_c, .d => .key_d, .e => .key_e,
        .f => .key_f, .g => .key_g, .h => .key_h, .i => .key_i, .j => .key_j,
        .k => .key_k, .l => .key_l, .m => .key_m, .n => .key_n, .o => .key_o,
        .p => .key_p, .q => .key_q, .r => .key_r, .s => .key_s, .t => .key_t,
        .u => .key_u, .v => .key_v, .w => .key_w, .x => .key_x, .y => .key_y, .z => .key_z,
        .digit_0 => .digit_0, .digit_1 => .digit_1, .digit_2 => .digit_2,
        .digit_3 => .digit_3, .digit_4 => .digit_4, .digit_5 => .digit_5,
        .digit_6 => .digit_6, .digit_7 => .digit_7, .digit_8 => .digit_8,
        .digit_9 => .digit_9,
        .backquote => .backquote, .backslash => .backslash,
        .bracket_left => .bracket_left, .bracket_right => .bracket_right,
        .comma => .comma, .equal => .equal, .minus => .minus,
        .period => .period, .quote => .quote, .semicolon => .semicolon,
        .slash => .slash,
        .enter => .enter, .tab => .tab, .space => .space,
        .backspace => .backspace, .escape => .escape,
        .insert => .insert, .delete => .delete, .home => .home, .end => .end,
        .page_up => .page_up, .page_down => .page_down,
        .arrow_up => .arrow_up, .arrow_down => .arrow_down,
        .arrow_left => .arrow_left, .arrow_right => .arrow_right,
        .f1 => .f1, .f2 => .f2, .f3 => .f3, .f4 => .f4, .f5 => .f5,
        .f6 => .f6, .f7 => .f7, .f8 => .f8, .f9 => .f9, .f10 => .f10,
        .f11 => .f11, .f12 => .f12,
        .shift_left => .shift_left, .shift_right => .shift_right,
        .control_left => .control_left, .control_right => .control_right,
        .alt_left => .alt_left, .alt_right => .alt_right,
        .meta_left => .meta_left, .meta_right => .meta_right,
        .caps_lock => .caps_lock,
    };
}

const KmuxEventSink = extern struct {
    user: ?*anyopaque,
    on_title: ?*const fn (*anyopaque, [*]const u8, usize) callconv(.c) void,
    on_bell: ?*const fn (*anyopaque) callconv(.c) void,
    on_osc52: ?*const fn (*anyopaque, u8, [*]const u8, usize) callconv(.c) void,
    on_hyperlink: ?*const fn (*anyopaque, [*]const u8, usize, [*]const u8, usize) callconv(.c) void,
};

// -----------------------------------------------------------------------------
// Handler — wraps ghostty's readonly handler, intercepting the four events we
// care about. Stored by value inside the `Stream`; its `terminal`/`sink`
// pointers stay valid because `Wrapper` lives on the heap and is never moved.
// -----------------------------------------------------------------------------

const Handler = struct {
    terminal: *Terminal,
    sink: *const KmuxEventSink,
    title_buf: *[1024]u8,
    title_len: *usize,

    pub fn deinit(self: *Handler) void {
        _ = self;
    }

    pub fn vt(
        self: *Handler,
        comptime action: Action.Tag,
        value: Action.Value(action),
    ) !void {
        switch (action) {
            .window_title => {
                // Store in the Wrapper-owned buffer so callers can pull the
                // current title at any time via kmux_ghostty_get_title.
                const n = @min(value.title.len, self.title_buf.len);
                @memcpy(self.title_buf[0..n], value.title[0..n]);
                self.title_len.* = n;
                // Also fire the callback for push-based consumers.
                if (self.sink.on_title) |cb| {
                    if (self.sink.user) |u| {
                        cb(u, value.title.ptr, value.title.len);
                    }
                }
                // Don't delegate: ghostty's readonly handler drops title.
            },
            .bell => {
                if (self.sink.on_bell) |cb| {
                    if (self.sink.user) |u| cb(u);
                }
            },
            .clipboard_contents => {
                if (self.sink.on_osc52) |cb| {
                    if (self.sink.user) |u| {
                        cb(u, value.kind, value.data.ptr, value.data.len);
                    }
                }
            },
            .start_hyperlink => {
                if (self.sink.on_hyperlink) |cb| {
                    if (self.sink.user) |u| {
                        const id = value.id orelse "";
                        cb(u, id.ptr, id.len, value.uri.ptr, value.uri.len);
                    }
                }
                // Delegate so ghostty keeps hyperlink cell-tracking state.
                var inner = gvt.ReadonlyHandler.init(self.terminal);
                defer inner.deinit();
                try inner.vt(action, value);
            },
            else => {
                var inner = gvt.ReadonlyHandler.init(self.terminal);
                defer inner.deinit();
                try inner.vt(action, value);
            },
        }
    }
};

const HandlerStream = gvt.Stream(Handler);

// -----------------------------------------------------------------------------
// Wrapper — one per kmux pane. Heap-allocated so internal pointers stay stable.
// -----------------------------------------------------------------------------

const Wrapper = struct {
    alloc: std.mem.Allocator,
    terminal: Terminal,
    stream: HandlerStream,
    pixel_width: u16,
    pixel_height: u16,
    sink: KmuxEventSink,
    title_buf: [1024]u8,
    title_len: usize,

    fn create(alloc: std.mem.Allocator, size: KmuxSize, scrollback: u32, sink: KmuxEventSink) !*Wrapper {
        if (size.rows == 0 or size.cols == 0) return error.InvalidSize;

        const self = try alloc.create(Wrapper);
        errdefer alloc.destroy(self);

        self.* = .{
            .alloc = alloc,
            .terminal = undefined,
            .stream = undefined,
            .pixel_width = size.pixel_width,
            .pixel_height = size.pixel_height,
            .sink = sink,
            .title_buf = undefined,
            .title_len = 0,
        };

        self.terminal = try Terminal.init(alloc, .{
            .cols = @intCast(size.cols),
            .rows = @intCast(size.rows),
            .max_scrollback = scrollback,
        });
        errdefer self.terminal.deinit(alloc);

        self.stream = HandlerStream.initAlloc(alloc, .{
            .terminal = &self.terminal,
            .sink = &self.sink,
            .title_buf = &self.title_buf,
            .title_len = &self.title_len,
        });

        return self;
    }

    fn destroy(self: *Wrapper) void {
        self.stream.deinit();
        self.terminal.deinit(self.alloc);
        self.alloc.destroy(self);
    }
};

// -----------------------------------------------------------------------------
// Color resolution
// -----------------------------------------------------------------------------

inline fn rgbToU32(rgb: gvt.color.RGB) u32 {
    return (@as(u32, rgb.r) << 16) | (@as(u32, rgb.g) << 8) | @as(u32, rgb.b);
}

const ResolvedCell = struct {
    fg: gvt.color.RGB,
    bg: gvt.color.RGB,
    fg_is_default: bool,
    bg_is_default: bool,
};

fn resolveCell(
    cell: gvt.Cell,
    style: gvt.Style,
    palette: *const gvt.color.Palette,
    default_fg: gvt.color.RGB,
    default_bg: gvt.color.RGB,
) ResolvedCell {
    const fg_is_default = (style.fg_color == .none);
    const fg_rgb: gvt.color.RGB = switch (style.fg_color) {
        .none => default_fg,
        .palette => |idx| palette[idx],
        .rgb => |rgb| rgb,
    };

    var bg_is_default = true;
    var bg_rgb: gvt.color.RGB = default_bg;
    switch (cell.content_tag) {
        .bg_color_palette => {
            bg_rgb = palette[cell.content.color_palette];
            bg_is_default = false;
        },
        .bg_color_rgb => {
            const rgb = cell.content.color_rgb;
            bg_rgb = .{ .r = rgb.r, .g = rgb.g, .b = rgb.b };
            bg_is_default = false;
        },
        else => switch (style.bg_color) {
            .none => {},
            .palette => |idx| {
                bg_rgb = palette[idx];
                bg_is_default = false;
            },
            .rgb => |rgb| {
                bg_rgb = rgb;
                bg_is_default = false;
            },
        },
    }

    // INVERSE swaps displayed fg/bg and the matching default-flag semantics.
    if (style.flags.inverse) {
        return .{
            .fg = bg_rgb,
            .bg = fg_rgb,
            .fg_is_default = bg_is_default,
            .bg_is_default = fg_is_default,
        };
    }

    return .{
        .fg = fg_rgb,
        .bg = bg_rgb,
        .fg_is_default = fg_is_default,
        .bg_is_default = bg_is_default,
    };
}

fn cellAttrs(cell: gvt.Cell, style: gvt.Style, resolved: ResolvedCell) u16 {
    var bits: u16 = 0;
    if (style.flags.bold) bits |= ATTR_BOLD;
    if (style.flags.italic) bits |= ATTR_ITALIC;
    if (style.flags.underline != .none) bits |= ATTR_UNDERLINE;
    if (style.flags.strikethrough) bits |= ATTR_STRIKETHROUGH;
    if (style.flags.inverse) bits |= ATTR_INVERSE;
    if (style.flags.invisible) bits |= ATTR_HIDDEN;
    if (style.flags.faint) bits |= ATTR_DIM;
    if (style.flags.blink) bits |= ATTR_BLINK;
    if (cell.wide == .wide) bits |= ATTR_WIDE_CHAR;
    if (cell.wide == .spacer_tail) bits |= ATTR_WIDE_CHAR_SPACER;
    if (resolved.fg_is_default) bits |= ATTR_DEFAULT_FG;
    if (resolved.bg_is_default) bits |= ATTR_DEFAULT_BG;
    return bits;
}

fn cellWidth(cell: gvt.Cell) u8 {
    return switch (cell.wide) {
        .wide => 2,
        .spacer_tail, .spacer_head => 0,
        .narrow => 1,
    };
}

fn cellCodepoint(cell: gvt.Cell) u32 {
    return switch (cell.content_tag) {
        .codepoint, .codepoint_grapheme => @intCast(cell.content.codepoint),
        .bg_color_palette, .bg_color_rgb => @as(u32, ' '),
    };
}

// -----------------------------------------------------------------------------
// Grid extraction
// -----------------------------------------------------------------------------

const RowFill = struct {
    rows_filled: usize,
};

fn fillRows(
    wrapper: *const Wrapper,
    tag: gvt.point.Tag,
    out: []KmuxCell,
    cols: usize,
    max_rows: usize,
) RowFill {
    if (out.len == 0 or cols == 0 or max_rows == 0) return .{ .rows_filled = 0 };

    const term: *const Terminal = &wrapper.terminal;
    const palette: *const gvt.color.Palette = &term.colors.palette.current;
    const default_fg = term.colors.foreground.get() orelse FALLBACK_FG;
    const default_bg = term.colors.background.get() orelse FALLBACK_BG;

    // Non-const pointer is required by rowIterator (it's declared on
    // `*const PageList` but the resulting Pin machinery walks mutable pages).
    var pages = &term.screens.active.pages;

    const tl_point: gvt.point.Point = switch (tag) {
        .active => .{ .active = .{} },
        .viewport => .{ .viewport = .{} },
        .history => .{ .history = .{} },
        .screen => .{ .screen = .{} },
    };

    var row_it = pages.rowIterator(.right_down, tl_point, null);
    var row_idx: usize = 0;
    while (row_it.next()) |pin| : (row_idx += 1) {
        if (row_idx >= max_rows) break;
        const row_cells = pin.cells(.all);
        const take = @min(row_cells.len, cols);
        const row_base = row_idx * cols;
        for (row_cells[0..take], 0..) |cell, col| {
            const style = if (cell.style_id == 0) gvt.Style{} else pin.style(&cell);
            const resolved = resolveCell(cell, style, palette, default_fg, default_bg);
            const kc: KmuxCell = .{
                .codepoint = cellCodepoint(cell),
                .fg_rgba = rgbToU32(resolved.fg),
                .bg_rgba = rgbToU32(resolved.bg),
                .attrs = cellAttrs(cell, style, resolved),
                .width = cellWidth(cell),
                ._pad = 0,
            };
            const slot = row_base + col;
            if (slot >= out.len) break;
            out[slot] = kc;
        }
    }

    return .{ .rows_filled = row_idx };
}

fn readCursor(wrapper: *const Wrapper, out: *KmuxCursor) void {
    const term: *const Terminal = &wrapper.terminal;
    const cur = term.screens.active.cursor;
    const visible = term.modes.get(.cursor_visible);
    const shape: u8 = if (!visible) SHAPE_HIDDEN else switch (cur.cursor_style) {
        .block => SHAPE_BLOCK,
        .block_hollow => SHAPE_HOLLOW_BLOCK,
        .underline => SHAPE_UNDERLINE,
        .bar => SHAPE_BAR,
    };
    out.* = .{
        .row = cur.y,
        .col = cur.x,
        .shape = shape,
        .visible = @intFromBool(visible),
        ._pad = .{ 0, 0 },
    };
}

fn readModes(wrapper: *const Wrapper, out: *KmuxModes) void {
    const term: *const Terminal = &wrapper.terminal;
    var bits: u16 = 0;
    if (term.modes.get(.cursor_keys)) bits |= MODE_APP_CURSOR;
    if (term.modes.get(.bracketed_paste)) bits |= MODE_BRACKETED_PASTE;
    if (term.modes.get(.mouse_event_normal)) bits |= MODE_MOUSE_REPORT_CLICK;
    if (term.modes.get(.mouse_event_button)) bits |= MODE_MOUSE_DRAG;
    if (term.modes.get(.mouse_event_any)) bits |= MODE_MOUSE_MOTION;
    if (term.modes.get(.mouse_format_sgr)) bits |= MODE_SGR_MOUSE;
    out.* = .{ .bits = bits };
}

fn readSize(wrapper: *const Wrapper, out: *KmuxSize) void {
    out.* = .{
        .rows = wrapper.terminal.rows,
        .cols = wrapper.terminal.cols,
        .pixel_width = wrapper.pixel_width,
        .pixel_height = wrapper.pixel_height,
    };
}

// -----------------------------------------------------------------------------
// Exported C ABI
// -----------------------------------------------------------------------------

export fn kmux_ghostty_abi_version() callconv(.c) u32 {
    return ABI_VERSION;
}

export fn kmux_ghostty_new(
    size_in: *const KmuxSize,
    scrollback: u32,
    sink_in: *const KmuxEventSink,
    out: **Wrapper,
) callconv(.c) i32 {
    const wrapper = Wrapper.create(std.heap.c_allocator, size_in.*, scrollback, sink_in.*) catch |err| return switch (err) {
        error.InvalidSize => ERR_INVALID_SIZE,
        else => ERR_ALLOC,
    };
    out.* = wrapper;
    return OK;
}

export fn kmux_ghostty_free(wrapper: ?*Wrapper) callconv(.c) void {
    if (wrapper) |w| w.destroy();
}

/// Copy the current window title into `out[0..buf_len]`.
/// Returns the number of bytes written (0 means no title has been set yet).
/// Does NOT NUL-terminate; the Rust side uses the returned length.
export fn kmux_ghostty_get_title(
    wrapper: *const Wrapper,
    out: [*]u8,
    buf_len: usize,
) callconv(.c) usize {
    const n = @min(wrapper.title_len, buf_len);
    @memcpy(out[0..n], wrapper.title_buf[0..n]);
    return n;
}

export fn kmux_ghostty_feed(
    wrapper: *Wrapper,
    ptr: [*]const u8,
    len: usize,
) callconv(.c) i32 {
    wrapper.stream.nextSlice(ptr[0..len]) catch return ERR_FEED;
    return OK;
}

export fn kmux_ghostty_resize(
    wrapper: *Wrapper,
    size_in: *const KmuxSize,
) callconv(.c) i32 {
    if (size_in.rows == 0 or size_in.cols == 0) return ERR_INVALID_SIZE;
    wrapper.terminal.resize(wrapper.alloc, size_in.cols, size_in.rows) catch return ERR_RESIZE;
    wrapper.pixel_width = size_in.pixel_width;
    wrapper.pixel_height = size_in.pixel_height;
    return OK;
}

export fn kmux_ghostty_size(wrapper: *const Wrapper, out: *KmuxSize) callconv(.c) void {
    readSize(wrapper, out);
}

export fn kmux_ghostty_fill_cells(
    wrapper: *const Wrapper,
    cells_ptr: [*]KmuxCell,
    cells_len: usize,
) callconv(.c) i32 {
    const cols: usize = wrapper.terminal.cols;
    const rows: usize = wrapper.terminal.rows;
    if (cells_len < rows * cols) return ERR_BAD_BUFFER;

    const buf = cells_ptr[0..cells_len];
    // Pre-fill with blanks so cells that ghostty never visits read as empty.
    for (buf[0 .. rows * cols]) |*c| c.* = .{
        .codepoint = ' ',
        .fg_rgba = rgbToU32(FALLBACK_FG),
        .bg_rgba = rgbToU32(FALLBACK_BG),
        .attrs = ATTR_DEFAULT_FG | ATTR_DEFAULT_BG,
        .width = 1,
        ._pad = 0,
    };
    _ = fillRows(wrapper, .active, buf, cols, rows);
    return OK;
}

export fn kmux_ghostty_fill_cells_and_cursor(
    wrapper: *const Wrapper,
    cells_ptr: [*]KmuxCell,
    cells_len: usize,
    cursor_out: *KmuxCursor,
    modes_out: *KmuxModes,
) callconv(.c) i32 {
    const rc = kmux_ghostty_fill_cells(wrapper, cells_ptr, cells_len);
    if (rc != OK) return rc;
    readCursor(wrapper, cursor_out);
    readModes(wrapper, modes_out);
    return OK;
}

export fn kmux_ghostty_cursor(wrapper: *const Wrapper, out: *KmuxCursor) callconv(.c) void {
    readCursor(wrapper, out);
}

export fn kmux_ghostty_modes(wrapper: *const Wrapper, out: *KmuxModes) callconv(.c) void {
    readModes(wrapper, out);
}

export fn kmux_ghostty_is_alt_screen(wrapper: *const Wrapper) callconv(.c) bool {
    return wrapper.terminal.screens.active_key == .alternate;
}

export fn kmux_ghostty_history_size(wrapper: *const Wrapper) callconv(.c) usize {
    const pages = &wrapper.terminal.screens.active.pages;
    // `total_rows` tracks every row currently held across all pages (active +
    // scrollback); `rows` on the PageList is just the active-viewport height.
    const total: usize = pages.total_rows;
    const visible: usize = wrapper.terminal.rows;
    return if (total > visible) total - visible else 0;
}

export fn kmux_ghostty_read_history(
    wrapper: *const Wrapper,
    start: usize,
    count: usize,
    cols: usize,
    cells_ptr: [*]KmuxCell,
    cells_len: usize,
    out_rows_filled: *usize,
) callconv(.c) i32 {
    out_rows_filled.* = 0;
    if (cols == 0 or count == 0) return OK;
    if (cells_len < count * cols) return ERR_BAD_BUFFER;

    const total = kmux_ghostty_history_size(wrapper);
    if (start >= total) return OK;

    const capped_count = @min(count, total - start);
    if (capped_count == 0) return OK;

    const term: *const Terminal = &wrapper.terminal;
    var pages = &term.screens.active.pages;
    const palette: *const gvt.color.Palette = &term.colors.palette.current;
    const default_fg = term.colors.foreground.get() orelse FALLBACK_FG;
    const default_bg = term.colors.background.get() orelse FALLBACK_BG;

    const buf = cells_ptr[0..cells_len];
    // Blank target region.
    for (buf[0 .. capped_count * cols]) |*c| c.* = .{
        .codepoint = ' ',
        .fg_rgba = rgbToU32(default_fg),
        .bg_rgba = rgbToU32(default_bg),
        .attrs = ATTR_DEFAULT_FG | ATTR_DEFAULT_BG,
        .width = 1,
        ._pad = 0,
    };

    var row_it = pages.rowIterator(.right_down, .{ .history = .{} }, null);

    // Skip `start` rows.
    var skipped: usize = 0;
    while (skipped < start) : (skipped += 1) {
        if (row_it.next() == null) {
            out_rows_filled.* = 0;
            return OK;
        }
    }

    var filled: usize = 0;
    while (filled < capped_count) {
        const pin = row_it.next() orelse break;
        const row_cells = pin.cells(.all);
        const take = @min(row_cells.len, cols);
        const base = filled * cols;
        for (row_cells[0..take], 0..) |cell, col| {
            const style = if (cell.style_id == 0) gvt.Style{} else pin.style(&cell);
            const resolved = resolveCell(cell, style, palette, default_fg, default_bg);
            buf[base + col] = .{
                .codepoint = cellCodepoint(cell),
                .fg_rgba = rgbToU32(resolved.fg),
                .bg_rgba = rgbToU32(resolved.bg),
                .attrs = cellAttrs(cell, style, resolved),
                .width = cellWidth(cell),
                ._pad = 0,
            };
        }
        filled += 1;
    }

    out_rows_filled.* = filled;
    return OK;
}

// -----------------------------------------------------------------------------
// Key encoding
// -----------------------------------------------------------------------------

/// Read the kitty keyboard protocol flags currently active on the inner
/// terminal (the `screens.active.kitty_keyboard.current()` value).  Returns
/// the packed `KittyFlags` u5 widened to u8 so it can travel as a stable
/// scalar across FFI.  Bits: 0=disambiguate, 1=report_events,
/// 2=report_alternates, 3=report_all, 4=report_associated.
export fn kmux_ghostty_kitty_flags(wrapper: *const Wrapper) callconv(.c) u8 {
    const flags = wrapper.terminal.screens.active.kitty_keyboard.current();
    return @intCast(flags.int());
}

/// Encode a single key event into terminal escape bytes using Ghostty's
/// `key_encode.encode`. Self-contained — the caller passes raw key/mods/
/// utf8/options as primitives and gets bytes back, no opaque encoder or
/// event handles needed.
///
/// `key`: ordinal of `KmuxKey` (kmux-stable enum, translated internally).
/// `action`: 0=release, 1=press, 2=repeat (`gvt.input.KeyAction`).
/// `mods`: packed `gvt.input.KeyMods` bits — bit 0=shift, 1=ctrl, 2=alt,
///   3=super, 4=caps_lock, 5=num_lock.
/// `utf8`: layout-dependent text the keystroke would produce when typed
///   in a plain text field. May be empty.
/// `unshifted_codepoint`: codepoint when no shift is applied (used by
///   kitty alternates). 0 if unknown.
/// `out_buf` / `out_buf_len`: caller's output buffer.
/// `out_written`: bytes written on success, or required size on
///   `ENC_OUT_OF_MEMORY`.
///
/// Returns `ENC_OK`, `ENC_OUT_OF_MEMORY`, or `ENC_INVALID_ENUM`.
export fn kmux_ghostty_encode_key(
    opts_in: *const KmuxKeyEncodeOptions,
    key: u16,
    mods: u16,
    action: u8,
    utf8_ptr: ?[*]const u8,
    utf8_len: usize,
    unshifted_codepoint: u32,
    out_buf: ?[*]u8,
    out_buf_len: usize,
    out_written: *usize,
) callconv(.c) i32 {
    out_written.* = 0;

    // Validate enum ordinals before unsafe casts.  Out-of-range values would
    // otherwise UB-cast to nonsense enum values that crash the encoder.
    const kmux_key = std.meta.intToEnum(KmuxKey, key) catch return ENC_INVALID_ENUM;
    const action_enum = std.meta.intToEnum(gvt.input.KeyAction, action) catch return ENC_INVALID_ENUM;
    const key_enum = kmuxKeyToGvt(kmux_key);

    // Build options.  KittyFlags is `packed struct(u5)`, so we truncate
    // any high bits the caller may have passed by mistake.
    const kitty_bits: u5 = @truncate(opts_in.kitty_flags);
    const opts: gvt.input.KeyEncodeOptions = .{
        .cursor_key_application = opts_in.cursor_key_application != 0,
        .keypad_key_application = opts_in.keypad_key_application != 0,
        .ignore_keypad_with_numlock = opts_in.ignore_keypad_with_numlock != 0,
        .alt_esc_prefix = opts_in.alt_esc_prefix != 0,
        .modify_other_keys_state_2 = opts_in.modify_other_keys_state_2 != 0,
        .kitty_flags = @bitCast(kitty_bits),
        .macos_option_as_alt = .false,
    };

    const utf8: []const u8 = if (utf8_ptr) |p| p[0..utf8_len] else &.{};

    const event: gvt.input.KeyEvent = .{
        .action = action_enum,
        .key = key_enum,
        .mods = @bitCast(mods),
        .composing = false,
        .utf8 = utf8,
        .unshifted_codepoint = @intCast(unshifted_codepoint),
    };

    // Try direct write to the caller's buffer.
    const out_slice: []u8 = if (out_buf) |p| p[0..out_buf_len] else &.{};
    var writer: std.Io.Writer = .fixed(out_slice);
    gvt.input.encodeKey(&writer, event, opts) catch |err| switch (err) {
        // No space — re-encode into a discarding writer to compute the
        // required size and report it via `out_written`.
        error.WriteFailed => {
            var discarding: std.Io.Writer.Discarding = .init(&.{});
            gvt.input.encodeKey(&discarding.writer, event, opts) catch unreachable;
            out_written.* = @intCast(discarding.count);
            return ENC_OUT_OF_MEMORY;
        },
    };

    out_written.* = writer.end;
    return ENC_OK;
}

// -----------------------------------------------------------------------------
// Tests (Zig-side sanity — Rust carries the exhaustive suite.)
// -----------------------------------------------------------------------------

test "abi version is positive" {
    try std.testing.expect(ABI_VERSION > 0);
}

test "encode plain enter without kitty flags is CR" {
    const opts: KmuxKeyEncodeOptions = .{
        .cursor_key_application = 0,
        .keypad_key_application = 0,
        .ignore_keypad_with_numlock = 0,
        .alt_esc_prefix = 0,
        .modify_other_keys_state_2 = 0,
        .kitty_flags = 0,
        ._pad = .{ 0, 0 },
    };
    var buf: [16]u8 = undefined;
    var written: usize = 0;
    const rc = kmux_ghostty_encode_key(
        &opts,
        @intFromEnum(KmuxKey.enter),
        0,
        @intFromEnum(gvt.input.KeyAction.press),
        null,
        0,
        0,
        &buf,
        buf.len,
        &written,
    );
    try std.testing.expectEqual(ENC_OK, rc);
    try std.testing.expectEqualStrings("\r", buf[0..written]);
}

test "encode shift+enter with kitty disambiguate is CSI 13;2u" {
    const opts: KmuxKeyEncodeOptions = .{
        .cursor_key_application = 0,
        .keypad_key_application = 0,
        .ignore_keypad_with_numlock = 0,
        .alt_esc_prefix = 0,
        .modify_other_keys_state_2 = 0,
        .kitty_flags = 0b1, // disambiguate
        ._pad = .{ 0, 0 },
    };
    var buf: [32]u8 = undefined;
    var written: usize = 0;
    const shift_only: u16 = 0b1; // Mods{ .shift = true } — bit 0
    const rc = kmux_ghostty_encode_key(
        &opts,
        @intFromEnum(KmuxKey.enter),
        shift_only,
        @intFromEnum(gvt.input.KeyAction.press),
        null,
        0,
        0,
        &buf,
        buf.len,
        &written,
    );
    try std.testing.expectEqual(ENC_OK, rc);
    try std.testing.expectEqualStrings("\x1b[13;2u", buf[0..written]);
}

test "encode shift+tab without kitty is CSI Z" {
    const opts: KmuxKeyEncodeOptions = .{
        .cursor_key_application = 0,
        .keypad_key_application = 0,
        .ignore_keypad_with_numlock = 0,
        .alt_esc_prefix = 0,
        .modify_other_keys_state_2 = 0,
        .kitty_flags = 0,
        ._pad = .{ 0, 0 },
    };
    var buf: [16]u8 = undefined;
    var written: usize = 0;
    const shift_only: u16 = 0b1;
    const rc = kmux_ghostty_encode_key(
        &opts,
        @intFromEnum(KmuxKey.tab),
        shift_only,
        @intFromEnum(gvt.input.KeyAction.press),
        null,
        0,
        0,
        &buf,
        buf.len,
        &written,
    );
    try std.testing.expectEqual(ENC_OK, rc);
    try std.testing.expectEqualStrings("\x1b[Z", buf[0..written]);
}

test "encode invalid key ordinal returns INVALID_ENUM" {
    const opts: KmuxKeyEncodeOptions = .{
        .cursor_key_application = 0,
        .keypad_key_application = 0,
        .ignore_keypad_with_numlock = 0,
        .alt_esc_prefix = 0,
        .modify_other_keys_state_2 = 0,
        .kitty_flags = 0,
        ._pad = .{ 0, 0 },
    };
    var buf: [16]u8 = undefined;
    var written: usize = 0;
    const rc = kmux_ghostty_encode_key(
        &opts,
        65000,
        0,
        @intFromEnum(gvt.input.KeyAction.press),
        null,
        0,
        0,
        &buf,
        buf.len,
        &written,
    );
    try std.testing.expectEqual(ENC_INVALID_ENUM, rc);
}

test "wrapper roundtrip: feed hello, read cells" {
    const alloc = std.testing.allocator;
    const sink: KmuxEventSink = .{
        .user = null,
        .on_title = null,
        .on_bell = null,
        .on_osc52 = null,
        .on_hyperlink = null,
    };
    const w = try Wrapper.create(alloc, .{ .rows = 4, .cols = 20, .pixel_width = 0, .pixel_height = 0 }, 1000, sink);
    defer w.destroy();

    try w.stream.nextSlice("hello");

    var buf: [80]KmuxCell = undefined;
    _ = kmux_ghostty_fill_cells(w, &buf, buf.len);
    try std.testing.expectEqual(@as(u32, 'h'), buf[0].codepoint);
    try std.testing.expectEqual(@as(u32, 'e'), buf[1].codepoint);
    try std.testing.expectEqual(@as(u32, 'l'), buf[2].codepoint);
    try std.testing.expectEqual(@as(u32, 'l'), buf[3].codepoint);
    try std.testing.expectEqual(@as(u32, 'o'), buf[4].codepoint);
}

comptime {
    // Keep the ghostty-vt import live even when no test is run.
    _ = gvt.Terminal;
}
