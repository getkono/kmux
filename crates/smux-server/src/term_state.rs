use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    term::{Config, RenderableCursor, Term, TermMode, cell::Flags},
    vte::ansi::{Color, CursorShape as AlacCursorShape, NamedColor, Processor},
};
use smux_protocol::messages::{
    CellAttrs, CellColor, CellState, CursorShape, CursorState, DiffOp, GridSnapshot, TermModes,
    TerminalDiff,
};

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

/// Server-side terminal state backed by `alacritty_terminal::Term`.
///
/// Feeds raw PTY bytes through the VTE `Processor`, then computes cell-level
/// diffs against the previous frame. The diff is sent to clients instead of
/// raw bytes — N clients × 1 parse instead of N parses.
pub struct TermState {
    term: Term<VoidListener>,
    processor: Processor,
    prev_cells: Vec<CellState>,
    /// Reusable scratch buffer — avoids allocation per `compute_diff()` call.
    current_cells: Vec<CellState>,
    prev_cursor: CursorState,
    prev_modes: TermModes,
    /// Tracks which rows have non-default content in `prev_cells`, so
    /// `compute_diff` can skip scanning rows that are blank in both buffers.
    prev_nonempty_rows: Vec<bool>,
    rows: u16,
    cols: u16,
}

impl TermState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let r = rows as usize;
        let c = cols as usize;
        let dims = TermDims { rows: r, cols: c };
        let term = Term::new(Config::default(), &dims, VoidListener);
        let blank = CellState::default();
        Self {
            term,
            processor: Processor::new(),
            prev_cells: vec![blank; r * c],
            current_cells: vec![blank; r * c],
            prev_cursor: CursorState::default(),
            prev_modes: TermModes::EMPTY,
            prev_nonempty_rows: vec![false; r],
            rows,
            cols,
        }
    }

    /// Feed raw PTY output bytes through the VTE parser.
    pub fn feed(&mut self, data: &[u8]) {
        self.processor.advance(&mut self.term, data);
    }

    /// Compute a diff between the current grid and `prev_cells`, then update
    /// `prev_cells` to match the current grid.
    ///
    /// Uses dirty-row tracking to skip scanning rows that are blank in both
    /// the current and previous frame, reducing work for typical terminal
    /// activity where only a few rows change per update.
    pub fn compute_diff(&mut self) -> Option<TerminalDiff> {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        let content = self.term.renderable_content();
        let colors = content.colors;
        let cursor = content.cursor;
        let display_offset = content.display_offset as i32;

        // Track which rows are touched by display_iter (have non-default content now).
        let mut touched_rows = vec![false; rows];

        // Reset scratch buffer and populate from grid
        self.current_cells.fill(CellState::default());

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset;
            let col = indexed.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                touched_rows[row as usize] = true;
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
                self.current_cells[row as usize * cols + col] = CellState {
                    c: cell.c,
                    fg,
                    bg,
                    attrs: convert_flags(cell.flags),
                };
            }
        }

        // Only compare rows that have content now OR had content previously.
        // Rows that are blank in both frames cannot have changed.
        let mut ops = Vec::new();
        for (r, (&touched, &prev_nonempty)) in touched_rows
            .iter()
            .zip(self.prev_nonempty_rows.iter())
            .enumerate()
            .take(rows)
        {
            if !touched && !prev_nonempty {
                continue;
            }
            let base = r * cols;
            let mut c = 0;
            while c < cols {
                if self.current_cells[base + c] != self.prev_cells[base + c] {
                    let start = c;
                    c += 1;
                    while c < cols && self.current_cells[base + c] != self.prev_cells[base + c] {
                        c += 1;
                    }
                    let run_len = c - start;
                    if run_len >= 2 {
                        ops.push(DiffOp::Row {
                            row: r as u16,
                            start_col: start as u16,
                            cells: self.current_cells[base + start..base + c].to_vec(),
                        });
                    } else {
                        ops.push(DiffOp::Cell {
                            row: r as u16,
                            col: start as u16,
                            cell: self.current_cells[base + start],
                        });
                    }
                } else {
                    c += 1;
                }
            }
        }

        // Update prev_nonempty_rows for next frame
        self.prev_nonempty_rows.copy_from_slice(&touched_rows);

        // Swap buffers: current becomes prev for next frame
        std::mem::swap(&mut self.prev_cells, &mut self.current_cells);

        let cursor_state = Self::convert_cursor(&cursor, display_offset);
        let modes = self.extract_modes();

        let has_changes =
            !ops.is_empty() || cursor_state != self.prev_cursor || modes != self.prev_modes;

        self.prev_cursor = cursor_state;
        self.prev_modes = modes;

        if has_changes {
            Some(TerminalDiff {
                ops,
                cursor: cursor_state,
                modes,
            })
        } else {
            None
        }
    }

    /// Take a full grid snapshot (for initial attach or post-resize).
    pub fn snapshot(&self) -> GridSnapshot {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        let content = self.term.renderable_content();
        let colors = content.colors;
        let cursor = content.cursor;
        let display_offset = content.display_offset as i32;

        let blank = CellState::default();
        let mut cells = vec![blank; rows * cols];

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + display_offset;
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
                cells[row as usize * cols + col] = CellState {
                    c: cell.c,
                    fg,
                    bg,
                    attrs: convert_flags(cell.flags),
                };
            }
        }

        let cursor_state = Self::convert_cursor(&cursor, display_offset);
        let modes = self.extract_modes();

        GridSnapshot {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor: cursor_state,
            modes,
        }
    }

    /// Resize the terminal. Resets `prev_cells` so the next diff is full-grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.term.resize(TermDims {
            rows: rows as usize,
            cols: cols as usize,
        });
        let n = rows as usize * cols as usize;
        self.prev_cells = vec![CellState::default(); n];
        self.current_cells = vec![CellState::default(); n];
        self.prev_cursor = CursorState::default();
        self.prev_modes = TermModes::EMPTY;
        self.prev_nonempty_rows = vec![false; rows as usize];
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

    fn extract_modes(&self) -> TermModes {
        let mut bits: u16 = 0;
        if self.term.mode().contains(TermMode::APP_CURSOR) {
            bits |= TermModes::APP_CURSOR;
        }
        TermModes(bits)
    }
}

// ── Color resolution (moved from client) ──────────────────────────────────────

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

    #[test]
    fn feed_hello_produces_5_cell_diff() {
        let mut ts = TermState::new(24, 80);
        ts.feed(b"hello");
        let diff = ts.compute_diff().expect("expected Some diff");
        // "hello" is 5 chars on row 0 — should produce one Row op of length 5
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
        let mut ts = TermState::new(24, 80);
        ts.feed(b"\x1b[31mred");
        let diff = ts.compute_diff().expect("expected Some diff");
        // Find the 'r' cell
        let r_cell = diff
            .ops
            .iter()
            .find_map(|op| match op {
                DiffOp::Cell { cell, .. } if cell.c == 'r' => Some(*cell),
                DiffOp::Row { cells, .. } => cells.iter().find(|c| c.c == 'r').copied(),
                _ => None,
            })
            .expect("should find 'r' cell");
        assert_eq!(r_cell.fg, CellColor::new(0xe0, 0x6c, 0x75)); // One Dark red
    }

    #[test]
    fn no_op_feed_produces_none() {
        let mut ts = TermState::new(24, 80);
        // Compute initial diff (all blank)
        let _ = ts.compute_diff();
        // Feed nothing
        ts.feed(b"");
        assert!(ts.compute_diff().is_none());
    }

    #[test]
    fn cursor_move_without_cell_change_produces_some() {
        let mut ts = TermState::new(24, 80);
        ts.feed(b"hello");
        let _ = ts.compute_diff(); // consume first diff

        // Move cursor home without changing cells (CSI H = cursor home)
        ts.feed(b"\x1b[H");
        let diff = ts
            .compute_diff()
            .expect("cursor-only move should produce Some");
        assert!(diff.ops.is_empty(), "no cell changes expected");
        assert_eq!(diff.cursor.row, 0);
        assert_eq!(diff.cursor.col, 0);
    }

    #[test]
    fn mode_change_without_cell_change_produces_some() {
        let mut ts = TermState::new(24, 80);
        let _ = ts.compute_diff(); // consume initial diff

        // Enable application cursor keys mode (DECCKM)
        ts.feed(b"\x1b[?1h");
        let diff = ts
            .compute_diff()
            .expect("mode-only change should produce Some");
        assert!(diff.ops.is_empty(), "no cell changes expected");
        assert!(diff.modes.app_cursor());
    }

    #[test]
    fn resize_resets_prev_cells() {
        let mut ts = TermState::new(24, 80);
        ts.feed(b"hello");
        let _ = ts.compute_diff(); // consume initial diff
        ts.resize(30, 100);
        // After resize, prev_cells/cursor/modes are reset so the diff should
        // reflect the new state.
        assert_eq!(ts.rows, 30);
        assert_eq!(ts.cols, 100);
        // We don't assert specific ops since resize behavior depends on alacritty
        let _ = ts.compute_diff();
    }

    #[test]
    fn snapshot_captures_full_grid() {
        let mut ts = TermState::new(24, 80);
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
        let mut ts = TermState::new(24, 80);
        ts.feed(b"hello");
        let snap = ts.snapshot();
        // After "hello", cursor should be at row 0, col 5
        assert_eq!(snap.cursor.row, 0);
        assert_eq!(snap.cursor.col, 5);
    }

    #[test]
    fn second_feed_only_diffs_new_chars() {
        let mut ts = TermState::new(24, 80);
        ts.feed(b"hello");
        let _ = ts.compute_diff(); // consume first diff

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
        // " world" is 6 chars, but cursor movement may cause the count to vary
        assert!(
            total_cells >= 5,
            "expected at least 5 changed cells, got {total_cells}"
        );
    }
}
