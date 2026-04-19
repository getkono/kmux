mod scrollback;
mod selection;
pub use scrollback::ScrollbackBuffer;
pub use selection::{DEFAULT_BG, GridPos, MULTI_CLICK_TIMEOUT_MS, Selection, SelectionMode};

use kmux_protocol::messages::{
    CellAttrs, CellState, CursorState, DiffOp, GridSnapshot, TermModes, TerminalDiff,
};

pub const CELL_WIDTH: f32 = 8.0;
pub const CELL_HEIGHT: f32 = 16.0;

/// Number of visible, non-default cells at the head of a scrollback line.
///
/// Trailing default-background blanks are ignored so they don't inflate the
/// wrap count when a narrow-width viewport renders them.
pub fn effective_line_len(line: &[CellState]) -> usize {
    for (i, cell) in line.iter().enumerate().rev() {
        let blank = cell.c == ' ' && cell.attrs.contains(CellAttrs::DEFAULT_BG);
        if !blank {
            return i + 1;
        }
    }
    0
}

/// Number of display rows a scrollback line occupies when rendered at a
/// viewport `cols` wide. Minimum is 1 (so blank lines still take a row).
pub fn display_rows_for_line(line: &[CellState], cols: usize) -> usize {
    if cols == 0 {
        return 1;
    }
    let eff = effective_line_len(line);
    if eff == 0 { 1 } else { eff.div_ceil(cols) }
}

/// Locate the scrollback display row at reverse offset `rev_offset` from the
/// newest row (0 = bottom of scrollback / just above live viewport).
///
/// Returns `(line_index, col_start)` where `col_start` is the absolute column
/// within the logical line at which this display row begins. The slice
/// `line[col_start .. col_start + cols]` (clamped to line length) is what
/// should be rendered.
pub fn scrollback_display_row_at(
    scrollback: &ScrollbackBuffer,
    cols: usize,
    rev_offset: usize,
) -> Option<(usize, usize)> {
    let mut remaining = rev_offset;
    for i in (0..scrollback.len()).rev() {
        let line = scrollback.get(i)?;
        let drs = display_rows_for_line(line, cols);
        if remaining < drs {
            let slice_idx = drs - 1 - remaining;
            return Some((i, slice_idx * cols));
        }
        remaining -= drs;
    }
    None
}

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
    /// Highest `history_total` the server has reported. When greater than
    /// `scrollback.history_total()`, there are scrollback lines the client has
    /// not yet received — the session manager will issue `FetchHistory` to
    /// fill the gap. Cleared when the gap closes.
    pending_history_total: Option<u64>,
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
            pending_history_total: None,
        }
    }

    /// Absolute `history_total` reported by the server but not yet satisfied
    /// by `ScrollbackAppend`/`HistoryLines`. `None` when the client is caught
    /// up. Sessions poll this to decide when to issue `FetchHistory`.
    pub fn pending_history_gap(&self) -> Option<(u64, u64)> {
        let have = self.scrollback.history_total();
        self.pending_history_total
            .and_then(|want| (want > have).then_some((have, want)))
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
    ///
    /// Rewrites the live viewport, cursor, and modes. Scrollback persists —
    /// snapshots are sent on attach and after resize, and in both cases the
    /// client's accumulated scrollback is still valid. The snapshot's
    /// `scrollback_tail` / `history_total` fields seed the buffer's absolute
    /// indices on fresh attach and reseat them on reattach. Only an explicit
    /// reset (see `clear()`) wipes it.
    pub fn apply_snapshot(&mut self, snapshot: GridSnapshot) {
        self.rows = snapshot.rows as usize;
        self.cols = snapshot.cols as usize;
        self.cells = snapshot.cells;
        self.cursor = snapshot.cursor;
        self.modes = snapshot.modes;
        if !snapshot.scrollback_tail.is_empty() || self.scrollback.is_empty() {
            self.scrollback
                .seed_tail(snapshot.history_total, snapshot.scrollback_tail);
        }
        if self
            .pending_history_total
            .is_some_and(|want| want <= self.scrollback.history_total())
        {
            self.pending_history_total = None;
        }
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Apply a diff from the server -- only changed cells are updated.
    ///
    /// Scrollback no longer travels with the diff (v16); it arrives out-of-band
    /// as `ScrollbackAppend`. `diff.history_total` is still used for
    /// monotonicity checks: if the server reports more history than the client
    /// has seen, we record the gap so the session manager can issue a
    /// `FetchHistory` request.
    pub fn apply_diff(&mut self, diff: TerminalDiff) {
        if diff.history_total > self.scrollback.history_total() {
            self.pending_history_total = Some(diff.history_total);
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

    /// Apply an out-of-band `ScrollbackAppend` from the daemon.
    ///
    /// `first_index` is the absolute index of the first line in `lines`. If
    /// the client's buffer has a gap (its `history_total()` is less than
    /// `first_index`), the buffer is cleared and a `FetchHistory` round-trip
    /// (driven by the session manager) will reseed it.
    pub fn apply_scrollback_append(&mut self, first_index: u64, lines: Vec<Vec<CellState>>) {
        if lines.is_empty() {
            return;
        }
        let new_count = lines.len();
        let old_len = self.scrollback.len();
        let ok = self.scrollback.append_with_index(first_index, lines);
        if !ok {
            self.scrollback.clear();
            self.selection = None;
        } else {
            let new_len = self.scrollback.len();
            let evicted = (old_len + new_count).saturating_sub(new_len);
            if let Some(sel) = &mut self.selection {
                if evicted > 0 && sel.anchor.row < evicted {
                    self.selection = None;
                } else if let Some(sel) = &mut self.selection {
                    let net = new_count - evicted;
                    sel.anchor.row = sel.anchor.row.saturating_sub(evicted) + net;
                    sel.end.row = sel.end.row.saturating_sub(evicted) + net;
                }
            }
        }
        if self
            .pending_history_total
            .is_some_and(|want| want <= self.scrollback.history_total())
        {
            self.pending_history_total = None;
        }
        self.cells_generation += 1;
    }

    /// Apply a `HistoryLines` reply. Used to fill gaps below the current
    /// `base_index` (older history the user scrolled into) or to recover
    /// from a detected gap. Only the portion newer than the current
    /// `history_total()` is appended; the rest is discarded (future work:
    /// support back-fill below `base_index` for deep scrollback).
    pub fn apply_history_lines(
        &mut self,
        first_index: u64,
        lines: Vec<Vec<CellState>>,
        _history_total: u64,
    ) {
        if lines.is_empty() {
            return;
        }
        let current_total = self.scrollback.history_total();
        let line_count = lines.len() as u64;
        let end_index = first_index + line_count;
        if first_index == current_total {
            let _ = self.scrollback.append_with_index(first_index, lines);
            self.cells_generation += 1;
        } else if first_index < current_total && end_index > current_total {
            let skip = (current_total - first_index) as usize;
            let tail: Vec<Vec<CellState>> = lines.into_iter().skip(skip).collect();
            if !tail.is_empty() {
                let _ = self.scrollback.append_with_index(current_total, tail);
                self.cells_generation += 1;
            }
        }
        // Else: reply is entirely older or duplicate; nothing to do until
        // back-fill support lands in Phase C.
        if self
            .pending_history_total
            .is_some_and(|want| want <= self.scrollback.history_total())
        {
            self.pending_history_total = None;
        }
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
        self.pending_history_total = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Resize the grid (server will send a fresh snapshot after resize).
    ///
    /// Scrollback is intentionally preserved across resize. The viewport is
    /// zeroed and the incoming snapshot will populate it; previously-captured
    /// scrollback lines remain at the width they were captured and are
    /// wrap-rendered to the new viewport width.
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

    /// Total number of **display rows** the scrollback would occupy at the
    /// current viewport width. A single logical scrollback line that is
    /// wider than the viewport contributes `ceil(effective_len / cols)` rows;
    /// blank-suffix padding does not inflate the count.
    pub fn total_scrollback_display_rows(&self) -> usize {
        let cols = self.cols;
        let mut total = 0usize;
        for i in 0..self.scrollback.len() {
            if let Some(line) = self.scrollback.get(i) {
                total += display_rows_for_line(line, cols);
            }
        }
        total
    }

    /// Scroll up by `n` **display rows** into history. Capped at the total
    /// number of display rows currently in scrollback.
    pub fn scroll_up(&mut self, n: usize) {
        let max_offset = self.total_scrollback_display_rows();
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

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::CellColor;

    fn cell(c: char) -> CellState {
        CellState {
            c,
            fg: CellColor::new(0xff, 0xff, 0xff),
            bg: CellColor::new(0, 0, 0),
            attrs: CellAttrs::EMPTY,
        }
    }

    fn line(text: &str) -> Vec<CellState> {
        text.chars().map(cell).collect()
    }

    fn snapshot(rows: u16, cols: u16) -> GridSnapshot {
        GridSnapshot {
            rows,
            cols,
            cells: vec![CellState::default(); rows as usize * cols as usize],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_tail: Vec::new(),
        }
    }

    fn push_scrollback(grid: &mut CellGrid, lines: Vec<Vec<CellState>>) {
        let first_index = grid.scrollback().history_total();
        grid.apply_scrollback_append(first_index, lines);
    }

    #[test]
    fn apply_snapshot_preserves_scrollback() {
        let mut grid = CellGrid::new(24, 80);
        push_scrollback(&mut grid, vec![line("hello"), line("world")]);
        assert_eq!(grid.scrollback_len(), 2);

        grid.apply_snapshot(snapshot(24, 80));
        assert_eq!(
            grid.scrollback_len(),
            2,
            "snapshot must not wipe scrollback"
        );
        let first = grid.scrollback().get(0).expect("line 0 present");
        assert_eq!(first.len(), 5);
        assert_eq!(first[0].c, 'h');
    }

    #[test]
    fn resize_preserves_scrollback() {
        let mut grid = CellGrid::new(24, 80);
        push_scrollback(&mut grid, vec![line("hello")]);
        assert_eq!(grid.scrollback_len(), 1);

        grid.resize(40, 120);
        assert_eq!(grid.scrollback_len(), 1, "resize must not wipe scrollback");
        assert_eq!(grid.rows, 40);
        assert_eq!(grid.cols, 120);
    }

    #[test]
    fn clear_wipes_scrollback() {
        let mut grid = CellGrid::new(24, 80);
        push_scrollback(&mut grid, vec![line("hello")]);
        assert_eq!(grid.scrollback_len(), 1);

        grid.clear();
        assert_eq!(
            grid.scrollback_len(),
            0,
            "explicit clear() still wipes scrollback"
        );
    }

    #[test]
    fn apply_diff_flags_pending_history_gap() {
        let mut grid = CellGrid::new(24, 80);
        let diff = TerminalDiff {
            ops: vec![],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 5,
        };
        grid.apply_diff(diff);
        assert_eq!(grid.pending_history_gap(), Some((0, 5)));

        grid.apply_scrollback_append(
            0,
            vec![line("a"), line("b"), line("c"), line("d"), line("e")],
        );
        assert_eq!(grid.pending_history_gap(), None);
    }

    #[test]
    fn effective_line_len_ignores_trailing_default_blanks() {
        let mut padded = line("hi");
        padded.extend(vec![CellState::default(); 8]);
        assert_eq!(effective_line_len(&padded), 2);
    }

    #[test]
    fn display_rows_wraps_wide_line() {
        let wide = line(&"X".repeat(150));
        assert_eq!(display_rows_for_line(&wide, 80), 2);
        assert_eq!(display_rows_for_line(&wide, 200), 1);
        assert_eq!(display_rows_for_line(&line(""), 80), 1);
    }

    #[test]
    fn scrollback_display_row_walks_back_across_wraps() {
        let mut grid = CellGrid::new(24, 40);
        // Two logical lines; first wraps into 3 display rows at cols=40.
        let wide = line(&"X".repeat(100));
        push_scrollback(&mut grid, vec![wide, line("tail")]);

        assert_eq!(grid.total_scrollback_display_rows(), 4);

        // rev_offset 0 = the "tail" line (1 display row); line index 1.
        let (li, cs) = scrollback_display_row_at(grid.scrollback(), 40, 0).unwrap();
        assert_eq!(li, 1);
        assert_eq!(cs, 0);

        // rev_offset 1 = last slice of the wide line (cols 80..100).
        let (li, cs) = scrollback_display_row_at(grid.scrollback(), 40, 1).unwrap();
        assert_eq!(li, 0);
        assert_eq!(cs, 80);

        // rev_offset 3 = first slice of the wide line (cols 0..40).
        let (li, cs) = scrollback_display_row_at(grid.scrollback(), 40, 3).unwrap();
        assert_eq!(li, 0);
        assert_eq!(cs, 0);

        // rev_offset 4 = past the top.
        assert!(scrollback_display_row_at(grid.scrollback(), 40, 4).is_none());
    }

    #[test]
    fn scroll_up_caps_to_display_rows_not_logical_lines() {
        let mut grid = CellGrid::new(24, 40);
        push_scrollback(&mut grid, vec![line(&"X".repeat(100)), line("tail")]);
        grid.scroll_up(100);
        // Total = 4 display rows (3 for wrapped + 1 for tail); scroll cap = 4.
        assert_eq!(grid.scroll_offset(), 4);
    }
}
