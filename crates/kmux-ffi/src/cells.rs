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
use kmux_client::grid::{CellGrid, scrollback_display_row_at};
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

/// Pack the grid's *displayed* cells row-major into a flat buffer
/// (`rows * cols * PACKED_CELL_LEN` bytes).
///
/// When scrolled into history (`scroll_offset > 0`) this composites scrollback
/// lines into the top rows exactly like the GTK renderer (`render.rs::cell_at` +
/// [`scrollback_display_row_at`]), so the Swift frontend renders scrollback
/// content while scrolled — not just the live viewport. At the live bottom the
/// output is identical to packing `grid.cells()` directly. Positions with no
/// backing cell (a short scrollback slice) encode as a blank, palette-background
/// cell so the row still tiles fully.
pub fn encode_cells(grid: &CellGrid, theme: &Theme) -> Vec<u8> {
    let cols = grid.cols;
    let rows = grid.rows;
    let scroll_offset = grid.scroll_offset();
    let scrollback = grid.scrollback();
    let cells = grid.cells();
    let blank = CellState::default();

    let mut out = Vec::with_capacity(rows * cols * PACKED_CELL_LEN);
    for vr in 0..rows {
        let sb_row = if scroll_offset > 0 && vr < scroll_offset {
            scrollback_display_row_at(scrollback, cols, scroll_offset - 1 - vr)
        } else {
            None
        };
        for vc in 0..cols {
            let cell = if let Some((line_idx, col_start)) = sb_row {
                scrollback
                    .get(line_idx)
                    .and_then(|line| line.get(col_start + vc))
            } else if scroll_offset > 0 {
                vr.checked_sub(scroll_offset)
                    .and_then(|grid_row| cells.get(grid_row * cols + vc))
            } else {
                cells.get(vr * cols + vc)
            };
            encode_cell(&mut out, cell.unwrap_or(&blank), theme);
        }
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
        let off = (vr * cols + vc) * PACKED_CELL_LEN;
        let code = u32::from_le_bytes(packed[off..off + 4].try_into().unwrap());
        char::from_u32(code).unwrap()
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
        // At the live bottom (no scroll), output equals packing grid.cells().
        let t = theme();
        let packed = encode_cells(&grid, &t);
        let mut expected = Vec::new();
        for cell in grid.cells() {
            encode_cell(&mut expected, cell, &t);
        }
        assert_eq!(packed, expected);
    }
}
