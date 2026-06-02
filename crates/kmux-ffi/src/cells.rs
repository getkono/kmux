//! Packed, FFI-stable encoding of the terminal grid for the Swift renderer.
//!
//! The hot render path is the cell grid. Rather than lowering a `Vec` of typed
//! records across the FFI boundary every frame (uniffi would box each one), the
//! grid is packed into a single flat `Vec<u8>` whose layout is documented here
//! and reinterpreted on the Swift side — one allocation per *changed* frame
//! (the frontend skips the fetch when [`CellGrid::cells_generation`] is
//! unchanged). The `DEFAULT_FG`/`DEFAULT_BG` cell flags are resolved against the
//! active palette *in Rust*, so Swift receives final RGBA and never needs the
//! theme to paint a cell.

use kmux_app::theme::Theme;
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{CellAttrs, CellState, CursorShape};

/// Bytes per packed cell. Little-endian layout:
/// - `[0..4]`  Unicode scalar value (`char as u32`)
/// - `[4..8]`  foreground RGBA (`DEFAULT_FG` resolved against the palette)
/// - `[8..12]` background RGBA (`DEFAULT_BG` resolved against the palette)
/// - `[12..14]` attribute bits (`CellAttrs`: bold=0, italic=1, underline=2,
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

fn cell_width(attrs: CellAttrs) -> u8 {
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

/// Pack the grid's visible cells row-major into a flat buffer
/// (`rows * cols * PACKED_CELL_LEN` bytes).
pub fn encode_cells(grid: &CellGrid, theme: &Theme) -> Vec<u8> {
    let cells = grid.cells();
    let mut out = Vec::with_capacity(cells.len() * PACKED_CELL_LEN);
    for cell in cells {
        encode_cell(&mut out, cell, theme);
    }
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
        // DEFAULT_FG/DEFAULT_BG → the palette colors, not the cell's stored RGB.
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
}
