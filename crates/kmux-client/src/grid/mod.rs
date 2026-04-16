mod scrollback;
mod selection;
pub use scrollback::ScrollbackBuffer;
pub use selection::{DEFAULT_BG, GridPos, MULTI_CLICK_TIMEOUT_MS, Selection, SelectionMode};

use kmux_protocol::messages::{
    CellAttrs, CellState, CursorState, DiffOp, GridSnapshot, TermModes, TerminalDiff,
};

pub const CELL_WIDTH: f32 = 8.0;
pub const CELL_HEIGHT: f32 = 16.0;

/// Client-side grid state -- receives pre-resolved cells from the server.
///
/// Unlike the old `TerminalBuffer` that wrapped `alacritty_terminal::Term`,
/// this is a thin grid of `CellState` values. All VT parsing and color
/// resolution happens on the server.
///
/// Rendering uses generation-based cache invalidation: the canvas cache is
/// rebuilt whenever `cells_generation` changes, guaranteeing that every
/// server-side change is reflected in the render.
pub struct CellGrid {
    cells: Vec<CellState>,
    cursor: CursorState,
    modes: TermModes,
    pub rows: usize,
    pub cols: usize,
    /// Incremented only when cell ops are non-empty, or on snapshot/clear/resize.
    cells_generation: u64,
    /// Incremented on every update (cell, cursor-only, snapshot, clear, resize).
    cursor_generation: u64,
    /// Lines that have scrolled off the top of the visible area.
    scrollback: ScrollbackBuffer,
    /// Scroll offset from the bottom (0 = live view, >0 = scrolled into history).
    scroll_offset: usize,
    /// Current text selection, if any.
    selection: Option<Selection>,
}

impl CellGrid {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            cells: vec![CellState::default(); rows * cols],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            rows,
            cols,
            cells_generation: 0,
            cursor_generation: 0,
            scrollback: ScrollbackBuffer::default(),
            scroll_offset: 0,
            selection: None,
        }
    }

    // ── Public accessors for renderers ──

    /// Access the flat cell buffer.
    pub fn cells(&self) -> &[CellState] {
        &self.cells
    }

    /// Current cursor state.
    pub fn cursor(&self) -> &CursorState {
        &self.cursor
    }

    /// Access the scrollback buffer.
    pub fn scrollback(&self) -> &ScrollbackBuffer {
        &self.scrollback
    }

    // ── State updates ──

    /// Replace the entire grid from a server snapshot.
    pub fn apply_snapshot(&mut self, snapshot: GridSnapshot) {
        self.rows = snapshot.rows as usize;
        self.cols = snapshot.cols as usize;
        self.cells = snapshot.cells;
        self.cursor = snapshot.cursor;
        self.modes = snapshot.modes;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Apply a diff from the server -- only changed cells are updated.
    pub fn apply_diff(&mut self, diff: TerminalDiff) {
        // Push scrollback lines before applying cell changes.
        if !diff.scrollback_lines.is_empty() {
            let new_count = diff.scrollback_lines.len();
            let old_len = self.scrollback.len();
            self.scrollback.push_lines(diff.scrollback_lines);
            let actual_added = self.scrollback.len() - old_len + new_count.min(old_len);
            // Evicted = lines that were dropped from the front of the ring buffer.
            let evicted = new_count.saturating_sub(actual_added);
            if let Some(sel) = &mut self.selection {
                // Shift selection rows up by evicted lines, then down by new lines.
                if evicted > 0 && sel.anchor.row < evicted {
                    self.selection = None;
                } else if let Some(sel) = &mut self.selection {
                    let net = new_count - evicted;
                    sel.anchor.row = sel.anchor.row.saturating_sub(evicted) + net;
                    sel.end.row = sel.end.row.saturating_sub(evicted) + net;
                }
            }
        }

        let has_cell_ops = !diff.ops.is_empty();
        for op in diff.ops {
            match op {
                DiffOp::Cell { row, col, cell } => {
                    let idx = row as usize * self.cols + col as usize;
                    if idx < self.cells.len() {
                        self.cells[idx] = cell;
                    }
                }
                DiffOp::Row {
                    row,
                    start_col,
                    cells,
                } => {
                    let base = row as usize * self.cols + start_col as usize;
                    for (i, cell) in cells.into_iter().enumerate() {
                        let idx = base + i;
                        if idx < self.cells.len() {
                            self.cells[idx] = cell;
                        }
                    }
                }
                DiffOp::Clear => {
                    self.cells.fill(CellState::default());
                }
            }
        }
        self.cursor = diff.cursor;
        self.modes = diff.modes;
        if has_cell_ops {
            self.cells_generation += 1;
        }
        self.cursor_generation += 1;
    }

    /// Apply a cursor-only update (no cell changes).
    pub fn apply_cursor_update(&mut self, cursor: CursorState, modes: TermModes) {
        self.cursor = cursor;
        self.modes = modes;
        self.cursor_generation += 1;
    }

    /// Whether the terminal is in application-cursor mode.
    pub fn app_cursor(&self) -> bool {
        self.modes.app_cursor()
    }

    /// Current terminal mode flags.
    pub fn modes(&self) -> TermModes {
        self.modes
    }

    /// Reset to blank cells.
    pub fn clear(&mut self) {
        self.cells.fill(CellState::default());
        self.cursor = CursorState::default();
        self.modes = TermModes::EMPTY;
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Resize the grid (server will send a fresh snapshot after resize).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.cells = vec![CellState::default(); self.rows * self.cols];
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Generation counter that changes on every update (used to detect changes).
    pub fn generation(&self) -> u64 {
        self.cursor_generation
    }

    /// Generation counter that changes only when cells change (used for cache invalidation).
    pub fn cells_generation(&self) -> u64 {
        self.cells_generation
    }

    /// Scroll up by `n` lines into history.
    pub fn scroll_up(&mut self, n: usize) {
        let max_offset = self.scrollback.len();
        let new_offset = (self.scroll_offset + n).min(max_offset);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.cells_generation += 1;
        }
    }

    /// Scroll down by `n` lines toward live view.
    pub fn scroll_down(&mut self, n: usize) {
        let new_offset = self.scroll_offset.saturating_sub(n);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.cells_generation += 1;
        }
    }

    /// Snap to the bottom (live view).
    pub fn scroll_to_bottom(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = 0;
            self.cells_generation += 1;
        }
    }

    /// Whether the view is scrolled up from live output.
    pub fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }

    /// Current scroll offset (0 = live, >0 = scrolled into history).
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Number of lines in scrollback history.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    // ── Selection ──

    pub fn selection(&self) -> Option<&Selection> {
        self.selection.as_ref()
    }

    pub fn set_selection(&mut self, sel: Option<Selection>) {
        self.selection = sel;
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Read the cell at an absolute grid position (scrollback + visible).
    pub fn cell_at(&self, pos: GridPos) -> Option<&CellState> {
        let sb_len = self.scrollback.len();
        if pos.row < sb_len {
            self.scrollback
                .get(pos.row)
                .and_then(|line| line.get(pos.col))
        } else {
            let grid_row = pos.row - sb_len;
            if grid_row < self.rows && pos.col < self.cols {
                self.cells.get(grid_row * self.cols + pos.col)
            } else {
                None
            }
        }
    }

    /// Find word boundaries around `pos` for double-click selection.
    pub fn find_word_boundaries(&self, pos: GridPos) -> (GridPos, GridPos) {
        let ch = self.cell_at(pos).map(|c| c.c).unwrap_or(' ');
        let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~');

        if is_word(ch) {
            let mut start_col = pos.col;
            while start_col > 0 {
                let prev = GridPos {
                    row: pos.row,
                    col: start_col - 1,
                };
                if self.cell_at(prev).map(|c| is_word(c.c)).unwrap_or(false) {
                    start_col -= 1;
                } else {
                    break;
                }
            }
            let max_col = self.cols.saturating_sub(1);
            let mut end_col = pos.col;
            while end_col < max_col {
                let next = GridPos {
                    row: pos.row,
                    col: end_col + 1,
                };
                if self.cell_at(next).map(|c| is_word(c.c)).unwrap_or(false) {
                    end_col += 1;
                } else {
                    break;
                }
            }
            (
                GridPos {
                    row: pos.row,
                    col: start_col,
                },
                GridPos {
                    row: pos.row,
                    col: end_col,
                },
            )
        } else {
            // Non-word: select just this character
            (pos, pos)
        }
    }

    /// Extract the text covered by the current selection.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let start = sel.start();
        let end = sel.end_pos();
        if start == end {
            return None;
        }

        let mut result = String::new();
        for row in start.row..=end.row {
            let col_start = if row == start.row { start.col } else { 0 };
            let col_end = if row == end.row {
                end.col
            } else {
                self.cols.saturating_sub(1)
            };

            let mut line = String::new();
            for col in col_start..=col_end {
                let pos = GridPos { row, col };
                if let Some(cell) = self.cell_at(pos) {
                    if cell.attrs.contains(CellAttrs::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    line.push(cell.c);
                }
            }
            // Trim trailing whitespace per line.
            let trimmed = line.trim_end();
            if row > start.row {
                result.push('\n');
            }
            result.push_str(trimmed);
        }
        Some(result)
    }
}

impl Default for CellGrid {
    fn default() -> Self {
        Self::new(24, 80)
    }
}
