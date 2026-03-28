use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{Config, RenderableCursor, Term, TermMode, cell::Flags},
    vte::ansi::{Color, CursorShape as AlacCursorShape, NamedColor, Processor},
};
use smux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, TermModes,
};

use super::TerminalBackend;

/// Grid dimensions wrapper implementing the alacritty `Dimensions` trait.
struct TermDims {
    rows: usize,
    cols: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// VT emulator backend powered by `alacritty_terminal::Term`.
pub struct AlacrittyBackend {
    term: Term<VoidListener>,
    processor: Processor,
    rows: u16,
    cols: u16,
}

impl AlacrittyBackend {
    pub fn new(rows: u16, cols: u16) -> Self {
        let dims = TermDims {
            rows: rows as usize,
            cols: cols as usize,
        };
        let term = Term::new(Config::default(), &dims, VoidListener);
        Self {
            term,
            processor: Processor::new(),
            rows,
            cols,
        }
    }

    fn convert_cursor(cursor: &RenderableCursor, display_offset: i32) -> CursorState {
        let row = cursor.point.line.0 + display_offset;
        CursorState {
            row: row.max(0) as u16,
            col: cursor.point.column.0 as u16,
            shape: convert_cursor_shape(cursor.shape),
            visible: cursor.shape != AlacCursorShape::Hidden,
        }
    }
}

impl TerminalBackend for AlacrittyBackend {
    fn feed(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    fn fill_cells(&self, out: &mut [CellState]) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let content = self.term.renderable_content();
        let colors = content.colors;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + content.display_offset as i32;
            let col = indexed.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                let cell = indexed.cell;
                let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
                    (
                        resolve_color(cell.bg, colors),
                        resolve_color(cell.fg, colors),
                    )
                } else {
                    (
                        resolve_color(cell.fg, colors),
                        resolve_color(cell.bg, colors),
                    )
                };
                out[row as usize * cols + col] = CellState {
                    c: cell.c,
                    fg,
                    bg,
                    attrs: convert_flags(cell.flags),
                };
            }
        }
    }

    fn cursor(&self) -> CursorState {
        let content = self.term.renderable_content();
        Self::convert_cursor(&content.cursor, content.display_offset as i32)
    }

    fn modes(&self) -> TermModes {
        let mut bits: u16 = 0;
        if self.term.mode().contains(TermMode::APP_CURSOR) {
            bits |= TermModes::APP_CURSOR;
        }
        TermModes(bits)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TermDims {
            rows: rows as usize,
            cols: cols as usize,
        });
    }
}

//  Color resolution helpers (One Dark palette)

fn resolve_color(color: Color, colors: &alacritty_terminal::term::color::Colors) -> CellColor {
    match color {
        Color::Named(name) => colors[name].map_or_else(
            || default_named_color(name),
            |rgb| CellColor::new(rgb.r, rgb.g, rgb.b),
        ),
        Color::Indexed(idx) => colors[idx as usize].map_or_else(
            || ansi_indexed_color(idx),
            |rgb| CellColor::new(rgb.r, rgb.g, rgb.b),
        ),
        Color::Spec(rgb) => CellColor::new(rgb.r, rgb.g, rgb.b),
    }
}

fn default_named_color(name: NamedColor) -> CellColor {
    match name {
        NamedColor::Black => CellColor::new(0x28, 0x2c, 0x34),
        NamedColor::Red => CellColor::new(0xe0, 0x6c, 0x75),
        NamedColor::Green => CellColor::new(0x98, 0xc3, 0x79),
        NamedColor::Yellow => CellColor::new(0xe5, 0xc0, 0x7b),
        NamedColor::Blue => CellColor::new(0x61, 0xaf, 0xef),
        NamedColor::Magenta => CellColor::new(0xc6, 0x78, 0xdd),
        NamedColor::Cyan => CellColor::new(0x56, 0xb6, 0xc2),
        NamedColor::White => CellColor::new(0xab, 0xb2, 0xbf),
        NamedColor::BrightBlack => CellColor::new(0x5c, 0x63, 0x70),
        NamedColor::BrightRed => CellColor::new(0xe0, 0x6c, 0x75),
        NamedColor::BrightGreen => CellColor::new(0x98, 0xc3, 0x79),
        NamedColor::BrightYellow => CellColor::new(0xe5, 0xc0, 0x7b),
        NamedColor::BrightBlue => CellColor::new(0x61, 0xaf, 0xef),
        NamedColor::BrightMagenta => CellColor::new(0xc6, 0x78, 0xdd),
        NamedColor::BrightCyan => CellColor::new(0x56, 0xb6, 0xc2),
        NamedColor::BrightWhite => CellColor::new(0xff, 0xff, 0xff),
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground => {
            CellColor::new(0xab, 0xb2, 0xbf)
        }
        NamedColor::Background => CellColor::new(0x28, 0x2c, 0x34),
        NamedColor::Cursor => CellColor::new(0x52, 0x8b, 0xff),
        NamedColor::DimBlack => CellColor::new(0x1e, 0x21, 0x27),
        NamedColor::DimRed => CellColor::new(0xa8, 0x51, 0x58),
        NamedColor::DimGreen => CellColor::new(0x72, 0x94, 0x5a),
        NamedColor::DimYellow => CellColor::new(0xac, 0x90, 0x5c),
        NamedColor::DimBlue => CellColor::new(0x49, 0x83, 0xb3),
        NamedColor::DimMagenta => CellColor::new(0x95, 0x5a, 0xa5),
        NamedColor::DimCyan => CellColor::new(0x40, 0x89, 0x91),
        NamedColor::DimWhite => CellColor::new(0x80, 0x87, 0x8f),
    }
}

fn ansi_indexed_color(idx: u8) -> CellColor {
    match idx {
        0..=15 => {
            let name = match idx {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                15 => NamedColor::BrightWhite,
                _ => unreachable!(),
            };
            default_named_color(name)
        }
        16..=231 => {
            let i = idx - 16;
            let r = (i / 36) % 6;
            let g = (i / 6) % 6;
            let b = i % 6;
            let to_byte = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
            CellColor::new(to_byte(r), to_byte(g), to_byte(b))
        }
        232..=255 => {
            let v = (idx - 232) * 10 + 8;
            CellColor::new(v, v, v)
        }
    }
}

fn convert_flags(flags: Flags) -> CellAttrs {
    let mut bits: u16 = 0;
    if flags.contains(Flags::BOLD) {
        bits |= CellAttrs::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        bits |= CellAttrs::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) || flags.contains(Flags::ALL_UNDERLINES) {
        bits |= CellAttrs::UNDERLINE;
    }
    if flags.contains(Flags::STRIKEOUT) {
        bits |= CellAttrs::STRIKETHROUGH;
    }
    if flags.contains(Flags::INVERSE) {
        bits |= CellAttrs::INVERSE;
    }
    if flags.contains(Flags::HIDDEN) {
        bits |= CellAttrs::HIDDEN;
    }
    if flags.contains(Flags::DIM) {
        bits |= CellAttrs::DIM;
    }
    CellAttrs(bits)
}

fn convert_cursor_shape(shape: AlacCursorShape) -> CursorShape {
    match shape {
        AlacCursorShape::Block => CursorShape::Block,
        AlacCursorShape::Underline => CursorShape::Underline,
        AlacCursorShape::Beam => CursorShape::Bar,
        AlacCursorShape::HollowBlock => CursorShape::HollowBlock,
        AlacCursorShape::Hidden => CursorShape::Hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff_engine::DiffEngine;
    use smux_protocol::messages::DiffOp;

    #[test]
    fn feed_hello_produces_5_cell_diff() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let diff = ts.compute_diff().expect("expected Some diff");
        let total_cells: usize = diff
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(
            total_cells >= 5,
            "expected at least 5 changed cells, got {total_cells}"
        );
    }

    #[test]
    fn feed_red_text_has_red_fg() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"\x1b[31mred");
        let diff = ts.compute_diff().expect("expected Some diff");
        let r_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'r' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'r').copied(),
                _ => None,
            })
            .expect("should find 'r' cell");
        assert_eq!(r_cell.fg, CellColor::new(0xe0, 0x6c, 0x75));
    }

    #[test]
    fn snapshot_captures_full_grid() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"ABC");
        let snap = ts.snapshot();
        assert_eq!(snap.rows, 24);
        assert_eq!(snap.cols, 80);
        assert_eq!(snap.cells.len(), 24 * 80);
        assert_eq!(snap.cells[0].c, 'A');
        assert_eq!(snap.cells[1].c, 'B');
        assert_eq!(snap.cells[2].c, 'C');
    }

    #[test]
    fn cursor_tracks_position() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let snap = ts.snapshot();
        assert_eq!(snap.cursor.row, 0);
        assert_eq!(snap.cursor.col, 5);
    }

    #[test]
    fn second_feed_only_diffs_new_chars() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let _ = ts.compute_diff();

        ts.feed(b" world");
        let diff = ts.compute_diff().expect("expected Some diff");
        let total_cells: usize = diff
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(
            total_cells >= 5,
            "expected at least 5 changed cells, got {total_cells}"
        );
    }

    #[test]
    fn fzf_highlight_move_produces_cell_diff() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"\x1b[?1049h\x1b[?1h\x1b[?25l");
        ts.feed(b"  item1\r\n");
        ts.feed(b"\x1b[7m> item2\x1b[27m\r\n");
        ts.feed(b"  item3\r\n");
        let _ = ts.compute_diff();

        ts.feed(b"\x1b[2;1H  item2");
        ts.feed(b"\x1b[1;1H\x1b[7m> item1\x1b[27m");
        let diff = ts
            .compute_diff()
            .expect("highlight move should produce diff");
        assert!(!diff.ops.is_empty(), "highlight move must have cell ops");
    }

    #[test]
    fn hello_cursor_move_world_diffs_correctly() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let diff1 = ts.compute_diff().expect("first diff");
        let cells1: usize = diff1
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(cells1 >= 5);

        ts.feed(b"\x1b[3;1H world");
        let diff2 = ts.compute_diff().expect("second diff");
        let cells2: usize = diff2
            .ops
            .iter()
            .map(|op| match op {
                DiffOp::Cell { .. } => 1,
                DiffOp::Row { cells, .. } => cells.len(),
                DiffOp::Clear => 0,
            })
            .sum();
        assert!(
            cells2 >= 5,
            "expected at least 5 changed cells on second diff, got {cells2}"
        );
    }
}
