use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line},
    term::{Config, RenderableCursor, Term, TermMode, cell::Flags},
    vte::ansi::{Color, CursorShape as AlacCursorShape, NamedColor, Processor},
};
use kmux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, TermModes,
};

use super::TerminalBackend;

/// Scrollback capacity per session (lines above the visible area).
const SCROLLBACK_LINES: usize = 50_000;

/// Grid dimensions wrapper implementing the alacritty `Dimensions` trait.
struct TermDims {
    rows: usize,
    cols: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.rows + SCROLLBACK_LINES
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
        let mode = self.term.mode();
        let mut bits: u16 = 0;
        if mode.contains(TermMode::APP_CURSOR) {
            bits |= TermModes::APP_CURSOR;
        }
        if mode.contains(TermMode::BRACKETED_PASTE) {
            bits |= TermModes::BRACKETED_PASTE;
        }
        if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            bits |= TermModes::MOUSE_REPORT_CLICK;
        }
        if mode.contains(TermMode::MOUSE_DRAG) {
            bits |= TermModes::MOUSE_DRAG;
        }
        if mode.contains(TermMode::MOUSE_MOTION) {
            bits |= TermModes::MOUSE_MOTION;
        }
        if mode.contains(TermMode::SGR_MOUSE) {
            bits |= TermModes::SGR_MOUSE;
        }
        TermModes(bits)
    }

    fn fill_cells_and_cursor(&self, out: &mut [CellState]) -> (CursorState, TermModes) {
        let rows = self.rows as usize;
        let cols = self.cols as usize;
        let content = self.term.renderable_content();
        let colors = content.colors;

        // Extract cursor and modes from the same RenderableContent.
        let cursor = Self::convert_cursor(&content.cursor, content.display_offset as i32);
        let mut mode_bits: u16 = 0;
        if content.mode.contains(TermMode::APP_CURSOR) {
            mode_bits |= TermModes::APP_CURSOR;
        }
        if content.mode.contains(TermMode::BRACKETED_PASTE) {
            mode_bits |= TermModes::BRACKETED_PASTE;
        }
        if content.mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            mode_bits |= TermModes::MOUSE_REPORT_CLICK;
        }
        if content.mode.contains(TermMode::MOUSE_DRAG) {
            mode_bits |= TermModes::MOUSE_DRAG;
        }
        if content.mode.contains(TermMode::MOUSE_MOTION) {
            mode_bits |= TermModes::MOUSE_MOTION;
        }
        if content.mode.contains(TermMode::SGR_MOUSE) {
            mode_bits |= TermModes::SGR_MOUSE;
        }
        let modes = TermModes(mode_bits);

        // Fill cells (same logic as fill_cells).
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

        (cursor, modes)
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TermDims {
            rows: rows as usize,
            cols: cols as usize,
        });
    }

    fn is_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    fn read_history_lines(&self, start: usize, count: usize, cols: usize) -> Vec<Vec<CellState>> {
        let grid = self.term.grid();
        let hist_size = grid.history_size();
        let colors = self.term.colors();
        let mut lines = Vec::with_capacity(count);

        for i in start..start.saturating_add(count).min(hist_size) {
            // History is indexed with negative Line values: Line(-1) is most
            // recent, Line(-hist_size) is oldest.  Index `i` counts from the
            // oldest, so map: line_idx = -(hist_size - i).
            let line_idx = -((hist_size - i) as i32);
            let row = &grid[Line(line_idx)];
            let mut cells = Vec::with_capacity(cols);
            for c in 0..cols {
                let alac_cell = &row[Column(c)];
                let (fg, bg) = if alac_cell.flags.contains(Flags::INVERSE) {
                    (
                        resolve_color(alac_cell.bg, colors),
                        resolve_color(alac_cell.fg, colors),
                    )
                } else {
                    (
                        resolve_color(alac_cell.fg, colors),
                        resolve_color(alac_cell.bg, colors),
                    )
                };
                cells.push(CellState {
                    c: alac_cell.c,
                    fg,
                    bg,
                    attrs: convert_flags(alac_cell.flags),
                });
            }
            lines.push(cells);
        }

        lines
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
    if flags.contains(Flags::WIDE_CHAR) {
        bits |= CellAttrs::WIDE_CHAR;
    }
    if flags.contains(Flags::WIDE_CHAR_SPACER) {
        bits |= CellAttrs::WIDE_CHAR_SPACER;
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
    use crate::diff_engine::{DiffEngine, DiffResult};
    use kmux_protocol::messages::DiffOp;

    /// Helper to extract a `TerminalDiff` from a `DiffResult::CellDiff`.
    fn expect_cell_diff(result: DiffResult) -> kmux_protocol::messages::TerminalDiff {
        match result {
            DiffResult::CellDiff(diff) => diff,
            other => panic!("expected CellDiff, got {other:?}"),
        }
    }

    #[test]
    fn feed_hello_produces_5_cell_diff() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let diff = expect_cell_diff(ts.compute_diff());
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
        let diff = expect_cell_diff(ts.compute_diff());
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
        let diff = expect_cell_diff(ts.compute_diff());
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
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(!diff.ops.is_empty(), "highlight move must have cell ops");
    }

    #[test]
    fn hello_cursor_move_world_diffs_correctly() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"hello");
        let diff1 = expect_cell_diff(ts.compute_diff());
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
        let diff2 = expect_cell_diff(ts.compute_diff());
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

    #[test]
    fn fzf_cursor_hidden_state() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        ts.feed(b"\x1b[?25l");
        let snap = ts.snapshot();
        assert!(
            !snap.cursor.visible,
            "cursor should be hidden after DECTCEM reset"
        );
    }

    #[test]
    fn bracketed_paste_mode_enable_disable() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));

        // Initially off
        assert!(
            !ts.modes().bracketed_paste(),
            "bracketed paste should be off by default"
        );

        // Enable DEC 2004
        ts.feed(b"\x1b[?2004h");
        assert!(
            ts.modes().bracketed_paste(),
            "bracketed paste should be on after \\e[?2004h"
        );

        // Disable DEC 2004
        ts.feed(b"\x1b[?2004l");
        assert!(
            !ts.modes().bracketed_paste(),
            "bracketed paste should be off after \\e[?2004l"
        );
    }

    #[test]
    fn mouse_report_click_mode_enable_disable() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));

        assert!(
            !ts.modes().mouse_report(),
            "mouse reporting should be off by default"
        );

        // Enable DEC 1000 (normal mouse tracking)
        ts.feed(b"\x1b[?1000h");
        assert!(
            ts.modes().mouse_report(),
            "mouse reporting should be on after \\e[?1000h"
        );
        assert!(!ts.modes().sgr_mouse(), "SGR mouse should still be off");

        // Enable SGR mouse (DEC 1006)
        ts.feed(b"\x1b[?1006h");
        assert!(
            ts.modes().sgr_mouse(),
            "SGR mouse should be on after \\e[?1006h"
        );

        // Disable both
        ts.feed(b"\x1b[?1000l\x1b[?1006l");
        assert!(
            !ts.modes().mouse_report(),
            "mouse reporting should be off after \\e[?1000l"
        );
        assert!(
            !ts.modes().sgr_mouse(),
            "SGR mouse should be off after \\e[?1006l"
        );
    }

    #[test]
    fn fzf_rapid_navigation_cycle() {
        let mut ts = DiffEngine::new(AlacrittyBackend::new(24, 80));
        // Set up alt screen with 5 items, item1 highlighted
        ts.feed(b"\x1b[?1049h\x1b[?25l");
        ts.feed(b"\x1b[7m> item1\x1b[27m\r\n");
        ts.feed(b"  item2\r\n");
        ts.feed(b"  item3\r\n");
        ts.feed(b"  item4\r\n");
        ts.feed(b"  item5\r\n");
        let _ = ts.compute_diff();

        // Navigate down 3 times, then up 2 times
        let moves = [
            // Down: unhighlight row 0, highlight row 1
            (&b"\x1b[1;1H  item1\x1b[2;1H\x1b[7m> item2\x1b[27m"[..]),
            (&b"\x1b[2;1H  item2\x1b[3;1H\x1b[7m> item3\x1b[27m"[..]),
            (&b"\x1b[3;1H  item3\x1b[4;1H\x1b[7m> item4\x1b[27m"[..]),
            // Up
            (&b"\x1b[4;1H  item4\x1b[3;1H\x1b[7m> item3\x1b[27m"[..]),
            (&b"\x1b[3;1H  item3\x1b[2;1H\x1b[7m> item2\x1b[27m"[..]),
        ];
        for (i, data) in moves.iter().enumerate() {
            ts.feed(data);
            let diff = expect_cell_diff(ts.compute_diff());
            assert!(
                !diff.ops.is_empty(),
                "navigation step {i} should produce cell ops"
            );
        }
    }

    #[test]
    fn alt_screen_no_scrollback_duplication() {
        let backend = AlacrittyBackend::new(4, 20);
        let mut ts = DiffEngine::new(backend);

        // Generate some scrollback by printing enough lines to overflow the 4-row screen.
        for i in 0..8 {
            ts.feed(format!("line {i}\r\n").as_bytes());
        }
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(
            !diff.scrollback_lines.is_empty(),
            "should have generated scrollback"
        );

        // Enter alt screen (SMCUP) and draw some content.
        ts.feed(b"\x1b[?1049h");
        ts.feed(b"fzf content");
        let diff = expect_cell_diff(ts.compute_diff());
        assert!(
            diff.scrollback_lines.is_empty(),
            "no scrollback on alt screen"
        );

        // Exit alt screen (RMCUP) -- should NOT re-send existing scrollback.
        ts.feed(b"\x1b[?1049l");
        let diff = ts.compute_diff();
        if let DiffResult::CellDiff(d) = diff {
            assert!(
                d.scrollback_lines.is_empty(),
                "exiting alt screen should not re-send {} scrollback lines",
                d.scrollback_lines.len()
            );
        }
        // CursorOnly or None are also acceptable.
    }
}
