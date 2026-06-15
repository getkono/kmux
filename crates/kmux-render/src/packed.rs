//! The single owner of kmux's FFI-stable packed-cell format.
//!
//! The hot render path is the cell grid. Rather than lowering a `Vec` of typed
//! records across the `kmux-ffi` boundary every frame (uniffi would box each
//! one), the grid is packed into a single flat `Vec<u8>` whose layout is
//! documented here and reinterpreted on the Swift side — one allocation per
//! *changed* frame (the frontend skips the fetch when
//! [`CellGrid::cells_generation`] is unchanged). The `DEFAULT_FG`/`DEFAULT_BG`
//! cell flags are resolved against the active palette *in Rust*, so a consumer
//! receives final RGBA and never needs the theme to paint a cell.
//!
//! This module previously lived in `kmux-ffi/src/cells.rs`. It moved here so the
//! format has ONE home guarded by [`crate::KMUX_RENDER_API_VERSION`]: `kmux-ffi`
//! re-exports these functions (so the Swift bindings are unchanged) and the GPU
//! renderer decodes the same bytes for its `Packed` cell source.
//!
//! [`CellGrid::cells_generation`]: kmux_client::grid::CellGrid::cells_generation

use kmux_app::theme::Theme;
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};

/// Bytes per packed cell. Little-endian layout:
/// - `[0..4]`  Unicode scalar value (`char as u32`)
/// - `[4..8]`  foreground RGBA (`DEFAULT_FG` resolved against the palette)
/// - `[8..12]` background RGBA (`DEFAULT_BG` resolved against the palette)
/// - `[12..14]` attribute bits ([`CellAttrs`]: bold=0, italic=1, underline=2,
///   strikethrough=3, inverse=4, hidden=5, dim=6, blink=7, wide=8,
///   wide-spacer=9; the `default_*` bits are already resolved away)
/// - `[14]` cell width: `0` = wide-char trailing spacer, `2` = wide char,
///   `1` = normal
/// - `[15]` reserved (zero)
pub const PACKED_CELL_LEN: usize = 16;

fn resolve_fg(cell: &CellState, theme: &Theme) -> [u8; 4] {
    if cell.attrs.contains(CellAttrs::DEFAULT_FG) {
        [theme.fg.r, theme.fg.g, theme.fg.b, 0xff]
    } else {
        [cell.fg.r, cell.fg.g, cell.fg.b, 0xff]
    }
}

fn resolve_bg(cell: &CellState, theme: &Theme) -> [u8; 4] {
    if cell.attrs.contains(CellAttrs::DEFAULT_BG) {
        [theme.bg.r, theme.bg.g, theme.bg.b, 0xff]
    } else {
        [cell.bg.r, cell.bg.g, cell.bg.b, 0xff]
    }
}

/// Cell width code: `0` = wide-char trailing spacer, `2` = wide char, `1` = normal.
pub fn cell_width(attrs: CellAttrs) -> u8 {
    if attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
        0
    } else if attrs.contains(CellAttrs::WIDE_CHAR) {
        2
    } else {
        1
    }
}

/// Append one packed cell ([`PACKED_CELL_LEN`] bytes) to `out`.
pub fn encode_cell(out: &mut Vec<u8>, cell: &CellState, theme: &Theme) {
    out.extend_from_slice(&(cell.c as u32).to_le_bytes());
    out.extend_from_slice(&resolve_fg(cell, theme));
    out.extend_from_slice(&resolve_bg(cell, theme));
    out.extend_from_slice(&cell.attrs.0.to_le_bytes());
    out.push(cell_width(cell.attrs));
    out.push(0); // reserved
}

/// Pack the grid's *displayed* cells row-major into a flat buffer
/// (`rows * cols * PACKED_CELL_LEN` bytes).
///
/// Scrollback compositing (when `scroll_offset > 0`) and the live-view fast path
/// are delegated to [`crate::geometry::for_each_displayed_cell`], the single
/// shared definition of "which cell shows at (vr, vc)", so this encoder and the
/// renderer's `Grid` path can never disagree.
pub fn encode_cells(grid: &CellGrid, theme: &Theme) -> Vec<u8> {
    let mut out = Vec::with_capacity(grid.rows * grid.cols * PACKED_CELL_LEN);
    crate::geometry::for_each_displayed_cell(grid, |_, _, cell| encode_cell(&mut out, cell, theme));
    out
}

/// Stable wire code for a cursor shape (matches the Swift `KmuxCursorShape`).
pub fn cursor_shape_code(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 0,
        CursorShape::Underline => 1,
        CursorShape::Bar => 2,
        CursorShape::HollowBlock => 3,
        CursorShape::Hidden => 4,
    }
}

/// A decoded packed cell: final (palette-resolved) colors + the character,
/// attributes, and width. The renderer's `Packed` cell source reads these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCell {
    /// The cell's character (`'\u{fffd}'` if the scalar was not a valid char).
    pub ch: char,
    /// Foreground RGBA, already resolved against the palette.
    pub fg: [u8; 4],
    /// Background RGBA, already resolved against the palette.
    pub bg: [u8; 4],
    /// Attribute bits (bold/italic/underline/…); the `default_*` bits are
    /// resolved away and should be ignored.
    pub attrs: CellAttrs,
    /// Width code: `0` = spacer, `1` = normal, `2` = wide.
    pub width: u8,
}

impl RenderCell {
    /// Whether this is the trailing spacer half of a wide character (no glyph).
    pub fn is_spacer(&self) -> bool {
        self.width == 0 || self.attrs.contains(CellAttrs::WIDE_CHAR_SPACER)
    }
}

/// Decode one [`PACKED_CELL_LEN`]-byte cell from the front of `bytes`.
///
/// # Panics
/// Panics if `bytes.len() < PACKED_CELL_LEN` (a packing/indexing bug).
pub fn decode_cell(bytes: &[u8]) -> RenderCell {
    let c: &[u8; PACKED_CELL_LEN] = bytes[..PACKED_CELL_LEN]
        .try_into()
        .expect("packed cell needs PACKED_CELL_LEN bytes");
    let scalar = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    RenderCell {
        ch: char::from_u32(scalar).unwrap_or('\u{fffd}'),
        fg: [c[4], c[5], c[6], c[7]],
        bg: [c[8], c[9], c[10], c[11]],
        attrs: CellAttrs(u16::from_le_bytes([c[12], c[13]])),
        width: c[14],
    }
}

/// Decode the cell at row-major `index` from a packed buffer.
///
/// # Panics
/// Panics if the buffer is too short for `index`.
pub fn decode_at(buf: &[u8], index: usize) -> RenderCell {
    let off = index * PACKED_CELL_LEN;
    decode_cell(&buf[off..off + PACKED_CELL_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_app::theme::{Rgb, Theme};
    use kmux_protocol::messages::CellColor;

    fn theme() -> Theme {
        let mut t = kmux_app::theme::default_theme();
        t.fg = Rgb::new(0xaa, 0xbb, 0xcc);
        t.bg = Rgb::new(0x11, 0x22, 0x33);
        t
    }

    #[test]
    fn explicit_colors_are_packed_verbatim() {
        let cell = CellState {
            c: 'A',
            fg: CellColor::new(10, 20, 30),
            bg: CellColor::new(40, 50, 60),
            attrs: CellAttrs(CellAttrs::BOLD),
        };
        let mut out = Vec::new();
        encode_cell(&mut out, &cell, &theme());
        assert_eq!(out.len(), PACKED_CELL_LEN);
        assert_eq!(&out[0..4], &0x41u32.to_le_bytes()); // 'A'
        assert_eq!(&out[4..8], &[10, 20, 30, 0xff]); // explicit fg
        assert_eq!(&out[8..12], &[40, 50, 60, 0xff]); // explicit bg
        assert_eq!(&out[12..14], &CellAttrs::BOLD.to_le_bytes()); // attrs
        assert_eq!(out[14], 1); // normal width
        assert_eq!(out[15], 0); // reserved
    }

    #[test]
    fn default_color_flags_resolve_against_theme() {
        let cell = CellState {
            c: ' ',
            fg: CellColor::new(0, 0, 0),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs(CellAttrs::DEFAULT_FG | CellAttrs::DEFAULT_BG),
        };
        let mut out = Vec::new();
        let t = theme();
        encode_cell(&mut out, &cell, &t);
        assert_eq!(&out[4..8], &[t.fg.r, t.fg.g, t.fg.b, 0xff]);
        assert_eq!(&out[8..12], &[t.bg.r, t.bg.g, t.bg.b, 0xff]);
    }

    #[test]
    fn wide_char_and_spacer_widths() {
        let mut wide = Vec::new();
        encode_cell(
            &mut wide,
            &CellState {
                c: '世',
                fg: CellColor::new(1, 1, 1),
                bg: CellColor::new(2, 2, 2),
                attrs: CellAttrs(CellAttrs::WIDE_CHAR),
            },
            &theme(),
        );
        assert_eq!(wide[14], 2, "wide char occupies two columns");

        let mut spacer = Vec::new();
        encode_cell(
            &mut spacer,
            &CellState {
                c: ' ',
                fg: CellColor::new(1, 1, 1),
                bg: CellColor::new(2, 2, 2),
                attrs: CellAttrs(CellAttrs::WIDE_CHAR_SPACER),
            },
            &theme(),
        );
        assert_eq!(spacer[14], 0, "spacer cell is zero-width");
    }

    #[test]
    fn cursor_shape_codes_are_stable() {
        assert_eq!(cursor_shape_code(CursorShape::Block), 0);
        assert_eq!(cursor_shape_code(CursorShape::Underline), 1);
        assert_eq!(cursor_shape_code(CursorShape::Bar), 2);
        assert_eq!(cursor_shape_code(CursorShape::HollowBlock), 3);
        assert_eq!(cursor_shape_code(CursorShape::Hidden), 4);
    }

    fn line(text: &str) -> Vec<CellState> {
        text.chars()
            .map(|c| CellState {
                c,
                fg: CellColor::new(0xff, 0xff, 0xff),
                bg: CellColor::new(0, 0, 0),
                attrs: CellAttrs::EMPTY,
            })
            .collect()
    }

    fn char_at(packed: &[u8], cols: usize, vr: usize, vc: usize) -> char {
        decode_at(packed, vr * cols + vc).ch
    }

    #[test]
    fn encode_cells_composites_scrollback_when_scrolled() {
        let mut grid = CellGrid::new(2, 4);
        grid.apply_scrollback_append(0, vec![line("AAAA"), line("BBBB")]);
        grid.scroll_up(1); // top row shows the newest scrollback line ("BBBB")
        let packed = encode_cells(&grid, &theme());
        assert_eq!(packed.len(), 2 * 4 * PACKED_CELL_LEN);
        // Row 0 is composited from scrollback; row 1 is live grid row 0 (blank).
        assert_eq!(char_at(&packed, 4, 0, 0), 'B');
        assert_eq!(char_at(&packed, 4, 0, 3), 'B');
        assert_eq!(char_at(&packed, 4, 1, 0), ' ');
    }

    #[test]
    fn encode_cells_live_view_matches_raw_cells() {
        let mut grid = CellGrid::new(2, 4);
        grid.apply_scrollback_append(0, vec![line("AAAA")]);
        let t = theme();
        let packed = encode_cells(&grid, &t);
        let mut expected = Vec::new();
        for cell in grid.cells() {
            encode_cell(&mut expected, cell, &t);
        }
        assert_eq!(packed, expected);
    }

    #[test]
    fn decode_round_trips_encode() {
        let cell = CellState {
            c: '世',
            fg: CellColor::new(10, 20, 30),
            bg: CellColor::new(40, 50, 60),
            attrs: CellAttrs(CellAttrs::BOLD | CellAttrs::WIDE_CHAR),
        };
        let mut out = Vec::new();
        encode_cell(&mut out, &cell, &theme());
        let decoded = decode_cell(&out);
        assert_eq!(decoded.ch, '世');
        assert_eq!(decoded.fg, [10, 20, 30, 0xff]);
        assert_eq!(decoded.bg, [40, 50, 60, 0xff]);
        assert!(decoded.attrs.contains(CellAttrs::BOLD));
        assert_eq!(decoded.width, 2);
        assert!(!decoded.is_spacer());
    }

    #[test]
    fn decode_spacer_is_recognized() {
        let cell = CellState {
            c: ' ',
            fg: CellColor::new(1, 1, 1),
            bg: CellColor::new(2, 2, 2),
            attrs: CellAttrs(CellAttrs::WIDE_CHAR_SPACER),
        };
        let mut out = Vec::new();
        encode_cell(&mut out, &cell, &theme());
        assert!(decode_cell(&out).is_spacer());
    }

    #[test]
    fn decode_invalid_scalar_is_replacement_char() {
        // A surrogate code point (0xD800) is not a valid char.
        let mut bytes = vec![0u8; PACKED_CELL_LEN];
        bytes[0..4].copy_from_slice(&0xD800u32.to_le_bytes());
        assert_eq!(decode_cell(&bytes).ch, '\u{fffd}');
    }
}
