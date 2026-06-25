mod scrollback;
mod selection;
pub use scrollback::ScrollbackBuffer;
pub use selection::{DEFAULT_BG, GridPos, MULTI_CLICK_TIMEOUT_MS, Selection, SelectionMode};

use kmux_protocol::messages::{
    CellAttrs, CellState, CursorState, DiffOp, GridSnapshot, ScrollbackLine, TermModes,
    TerminalDiff,
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
        // Drop any scrollback the daemon has since evicted or wiped (e.g. a
        // `clear` after we lagged). `scrollback_base` is its oldest serveable
        // index. This runs UNCONDITIONALLY -- outside the `seed_tail` guard
        // below -- because the leak case is "clear then resize": the client
        // holds non-empty scrollback and the post-clear snapshot tail is empty,
        // so `seed_tail` is skipped and the stale lines would otherwise survive.
        self.scrollback.evict_before(snapshot.scrollback_base);
        if !snapshot.scrollback_tail.is_empty() || self.scrollback.is_empty() {
            self.scrollback
                .seed_tail(snapshot.history_total, snapshot.scrollback_tail);
        }
        self.maybe_clear_history_gap();
        self.scroll_offset = 0;
        self.selection = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
    }

    /// Clear the pending-history-gap marker once the scrollback mirror has caught
    /// up to (or past) the total we were waiting for. Called after any operation
    /// that can grow `history_total()`.
    fn maybe_clear_history_gap(&mut self) {
        if self
            .pending_history_total
            .is_some_and(|want| want <= self.scrollback.history_total())
        {
            self.pending_history_total = None;
        }
    }

    /// Export the current grid as a [`GridSnapshot`] — the inverse of
    /// [`apply_snapshot`](Self::apply_snapshot).
    ///
    /// Re-serialises a live `CellGrid` so a fresh consumer can reconstruct the
    /// exact view with no upstream round-trip (issue #121: the federation daemon
    /// mints a snapshot for a newly-attaching GUI from its authoritative pane
    /// mirror). The full held scrollback travels as `scrollback_tail` so the
    /// consumer renders history immediately. The view-local `scroll_offset` and
    /// `selection` are intentionally excluded from the wire snapshot.
    pub fn to_snapshot(&self) -> GridSnapshot {
        GridSnapshot {
            rows: self.rows as u16,
            cols: self.cols as u16,
            cells: self.cells.clone(),
            cursor: self.cursor,
            modes: self.modes,
            history_total: self.scrollback.history_total(),
            scrollback_base: self.scrollback.base_index(),
            scrollback_tail: self.scrollback.tail(),
        }
    }

    /// Compute the live grid digest the daemon's `GridDigest` certifies — the
    /// visible grid, cursor, modes, and scrollback envelope, excluding the
    /// scrollback tail (see [`GridSnapshot::live_digest`]). Builds a tail-less
    /// snapshot so the held scrollback is never cloned on the check path.
    pub fn live_digest(&self) -> u128 {
        GridSnapshot {
            rows: self.rows as u16,
            cols: self.cols as u16,
            cells: self.cells.clone(),
            cursor: self.cursor,
            modes: self.modes,
            history_total: self.scrollback.history_total(),
            scrollback_base: self.scrollback.base_index(),
            scrollback_tail: Vec::new(),
        }
        .live_digest()
    }

    /// Apply a diff from the server -- only changed cells are updated.
    ///
    /// Scrollback no longer travels with the diff (v16); it arrives out-of-band
    /// as `ScrollbackAppend`. `diff.history_total` is still used for
    /// monotonicity checks: if the server reports more history than the client
    /// has seen, we record the gap so the session manager can issue a
    /// `FetchHistory` request.
    pub fn apply_diff(&mut self, diff: TerminalDiff) {
        // A scrollback wipe (`clear`'s CSI 3J, `RIS`) must drop history BEFORE
        // the gap check below, so any surviving lines arrive cleanly via the
        // out-of-band append (which the relay orders after this diff) and no
        // spurious gap is recorded. `history_total` stays monotonic across the
        // wipe, so `reset_to(base)` re-anchors at the daemon's new oldest index.
        let scrollback_reset = diff.scrollback_reset.is_some();
        if let Some(base) = diff.scrollback_reset {
            self.scrollback.reset_to(base);
            self.scroll_offset = 0;
            self.selection = None;
            self.pending_history_total = None;
        }

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
        if has_cell_ops || scrollback_reset {
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
    pub fn apply_scrollback_append(&mut self, first_index: u64, lines: Vec<ScrollbackLine>) {
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
        self.maybe_clear_history_gap();
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
        lines: Vec<ScrollbackLine>,
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
            let tail: Vec<ScrollbackLine> = lines.into_iter().skip(skip).collect();
            if !tail.is_empty() {
                let _ = self.scrollback.append_with_index(current_total, tail);
                self.cells_generation += 1;
            }
        }
        // Else: reply is entirely older or duplicate; nothing to do until
        // back-fill support lands in Phase C.
        self.maybe_clear_history_gap();
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

    /// For a visible display row `vr`, the absolute logical row it shows and the
    /// column offset of that display row within the logical line.
    ///
    /// While scrolled into history (`scroll_offset > 0`), the top rows show
    /// scrollback (a wide logical line wraps across several display rows, each
    /// with a different `base_col`); the rest show the live grid. Mirrors the
    /// row resolution in the renderers (`render.rs` / `ui/grid.rs`).
    fn abs_row_base_at_visible(&self, vr: usize) -> (usize, usize) {
        if self.scroll_offset > 0
            && vr < self.scroll_offset
            && let Some((line_idx, col_start)) =
                scrollback_display_row_at(&self.scrollback, self.cols, self.scroll_offset - 1 - vr)
        {
            return (line_idx, col_start);
        }
        let grid_row = vr.saturating_sub(self.scroll_offset);
        (self.scrollback.len() + grid_row, 0)
    }

    /// Map a *visible* viewport cell to an absolute [`GridPos`], accounting for
    /// the current scroll offset and scrollback line wrapping. Clamps `vr`/`vc`
    /// to the grid. This is the single source of truth for pointer → grid
    /// mapping across frontends (GTK `pos_at`, the FFI selection methods).
    pub fn visible_to_abs(&self, vr: usize, vc: usize) -> GridPos {
        let vr = vr.min(self.rows.saturating_sub(1));
        let vc = vc.min(self.cols.saturating_sub(1));
        let (row, base_col) = self.abs_row_base_at_visible(vr);
        GridPos {
            row,
            col: base_col + vc,
        }
    }

    /// The selected column span on each *visible* display row, as
    /// `(visible_row, col_start, col_end)` (inclusive, viewport columns).
    ///
    /// Intersects the active selection (in absolute coordinates) with each
    /// visible row's `(abs_row, base_col)` mapping, so it handles scrolled views
    /// and wrapped scrollback lines uniformly. Empty when there is no selection
    /// or it is degenerate (anchor == end). The single source of truth for the
    /// selection wash on every frontend.
    pub fn visible_selection_spans(&self) -> Vec<(u16, u16, u16)> {
        let Some(sel) = self.selection.as_ref() else {
            return Vec::new();
        };
        let (start, end) = (sel.start(), sel.end_pos());
        if start == end || self.cols == 0 {
            return Vec::new();
        }
        let mut spans = Vec::new();
        for vr in 0..self.rows {
            let (abs_row, base_col) = self.abs_row_base_at_visible(vr);
            if abs_row < start.row || abs_row > end.row {
                continue;
            }
            let row_first = base_col;
            let row_last = base_col + self.cols - 1;
            let lo = if abs_row == start.row {
                start.col.max(row_first)
            } else {
                row_first
            };
            let hi = if abs_row == end.row {
                end.col.min(row_last)
            } else {
                row_last
            };
            if hi < lo {
                continue;
            }
            spans.push((vr as u16, (lo - base_col) as u16, (hi - base_col) as u16));
        }
        spans
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
            // A wrapped scrollback line is wider than the viewport, so bound the
            // rightward scan by the logical line's length (not `cols`) — else a
            // double-click near the wrap point would truncate the word.
            let max_col = if pos.row < self.scrollback.len() {
                self.scrollback
                    .get(pos.row)
                    .map(|l| effective_line_len(l).saturating_sub(1))
                    .unwrap_or(0)
            } else {
                self.cols.saturating_sub(1)
            };
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

        let sb_len = self.scrollback.len();
        let mut result = String::new();
        for row in start.row..=end.row {
            let col_start = if row == start.row { start.col } else { 0 };
            // A non-terminal row spans its full width: for a (possibly wrapped)
            // scrollback line that is the logical line's effective length, so
            // wide lines copy whole; for a live row it is the viewport width.
            let col_end = if row == end.row {
                end.col
            } else if row < sb_len {
                self.scrollback
                    .get(row)
                    .map(|l| effective_line_len(l).saturating_sub(1))
                    .unwrap_or(0)
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
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        }
    }

    fn push_scrollback(grid: &mut CellGrid, lines: Vec<Vec<CellState>>) {
        let first_index = grid.scrollback().history_total();
        grid.apply_scrollback_append(first_index, lines.into_iter().map(Into::into).collect());
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
    fn to_snapshot_round_trips_through_apply_snapshot() {
        // Drive a grid to a non-trivial state: populated cells, a moved cursor,
        // non-empty modes, and several scrollback lines.
        let mut a = CellGrid::new(3, 4);
        a.apply_diff(TerminalDiff {
            ops: vec![DiffOp::Row {
                row: 0,
                start_col: 0,
                cells: line("abcd"),
            }],
            cursor: CursorState {
                row: 1,
                col: 2,
                blink: true,
                ..CursorState::default()
            },
            modes: TermModes(TermModes::BRACKETED_PASTE | TermModes::SGR_MOUSE),
            history_total: 0,
            scrollback_reset: None,
        });
        push_scrollback(&mut a, vec![line("old1"), line("old2"), line("old3")]);

        // Export, then re-import into a fresh (deliberately mis-sized) grid.
        let snap = a.to_snapshot();
        let mut b = CellGrid::new(1, 1);
        b.apply_snapshot(snap);

        // The reconstructed grid is observably identical to the original.
        assert_eq!(b.rows, a.rows);
        assert_eq!(b.cols, a.cols);
        assert_eq!(b.cells, a.cells);
        assert_eq!(b.cursor, a.cursor);
        assert_eq!(b.modes, a.modes);
        assert_eq!(
            b.scrollback().history_total(),
            a.scrollback().history_total()
        );
        assert_eq!(b.scrollback().base_index(), a.scrollback().base_index());
        assert_eq!(b.scrollback_len(), a.scrollback_len());
        for i in 0..a.scrollback_len() {
            assert_eq!(b.scrollback().get(i), a.scrollback().get(i), "line {i}");
        }

        // The desync oracle's contract: a reconstructed grid hashes identically
        // to its source snapshot. This is what lets the server certify a seqno
        // with `digest(server_snapshot)` and the client confirm with
        // `digest(client.to_snapshot())`.
        assert_eq!(
            a.to_snapshot().digest(),
            b.to_snapshot().digest(),
            "round-tripped grid must share the source digest"
        );
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
    fn apply_diff_scrollback_reset_wipes_scrollback() {
        let mut grid = CellGrid::new(24, 80);
        push_scrollback(&mut grid, vec![line("hello"), line("world")]);
        assert_eq!(grid.scrollback_len(), 2);
        grid.scroll_up(1);
        assert!(grid.is_scrolled());

        // A diff carrying scrollback_reset wipes history and snaps to live view.
        // history_total stays monotonic: the new base equals the old total.
        let diff = TerminalDiff {
            ops: vec![DiffOp::Clear],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 2,
            scrollback_reset: Some(2),
        };
        grid.apply_diff(diff);
        assert_eq!(
            grid.scrollback_len(),
            0,
            "scrollback_reset wipes client scrollback"
        );
        assert!(!grid.is_scrolled(), "reset snaps back to the live view");
        assert_eq!(
            grid.scrollback().history_total(),
            2,
            "history_total stays monotonic across the wipe"
        );
        assert_eq!(grid.pending_history_gap(), None, "no spurious gap recorded");
    }

    fn cell_diff(ops: Vec<DiffOp>) -> TerminalDiff {
        TerminalDiff {
            ops,
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        }
    }

    #[test]
    fn apply_diff_cell_op_writes_at_row_major_index() {
        let mut grid = CellGrid::new(24, 80);
        // Row-major: (row 2, col 3) is index 2*80 + 3 = 163.
        grid.apply_diff(cell_diff(vec![DiffOp::Cell {
            row: 2,
            col: 3,
            cell: cell('X'),
        }]));
        assert_eq!(grid.cells[163].c, 'X', "cell lands at row*cols + col");
        assert_ne!(grid.cells[162].c, 'X', "the previous column is untouched");
        assert_ne!(grid.cells[164].c, 'X', "the next column is untouched");
    }

    #[test]
    fn apply_diff_cell_op_past_the_end_is_dropped_not_panicking() {
        let mut grid = CellGrid::new(24, 80); // 1920 cells, valid indices 0..=1919
        grid.apply_diff(cell_diff(vec![DiffOp::Cell {
            row: 23,
            col: 79,
            cell: cell('Z'),
        }]));
        assert_eq!(grid.cells[1919].c, 'Z', "the last valid cell is written");
        // row 24, col 0 → index 1920 == len → out of bounds; must be dropped.
        grid.apply_diff(cell_diff(vec![DiffOp::Cell {
            row: 24,
            col: 0,
            cell: cell('!'),
        }]));
        assert_eq!(
            grid.cells.len(),
            1920,
            "an out-of-bounds cell op never grows or panics"
        );
    }

    #[test]
    fn apply_diff_row_op_writes_a_contiguous_run() {
        let mut grid = CellGrid::new(24, 80);
        // Row 1 from col 2 → base 1*80 + 2 = 82.
        grid.apply_diff(cell_diff(vec![DiffOp::Row {
            row: 1,
            start_col: 2,
            cells: line("abc"),
        }]));
        assert_eq!(grid.cells[82].c, 'a');
        assert_eq!(grid.cells[83].c, 'b');
        assert_eq!(grid.cells[84].c, 'c');
        assert_ne!(
            grid.cells[81].c, 'a',
            "the cell before the run is untouched"
        );

        // A run spilling one cell past the buffer end is truncated, not panicking.
        grid.apply_diff(cell_diff(vec![DiffOp::Row {
            row: 23,
            start_col: 79,
            cells: line("YZ"),
        }]));
        assert_eq!(
            grid.cells[1919].c, 'Y',
            "the in-bounds part of the run is written"
        );
        assert_eq!(
            grid.cells.len(),
            1920,
            "the overflowing cell is dropped, no panic"
        );
    }

    #[test]
    fn apply_diff_bumps_cells_generation_only_when_cells_change() {
        let mut grid = CellGrid::new(24, 80);

        // A diff with cell ops advances BOTH generations.
        grid.apply_diff(cell_diff(vec![DiffOp::Cell {
            row: 0,
            col: 0,
            cell: cell('a'),
        }]));
        assert_eq!(
            grid.cells_generation, 1,
            "cell ops bump the cells generation"
        );
        assert_eq!(
            grid.cursor_generation, 1,
            "every diff bumps the cursor generation"
        );

        // A diff with no ops (and no scrollback reset) leaves the cells
        // generation untouched but still advances the cursor generation.
        grid.apply_diff(cell_diff(vec![]));
        assert_eq!(
            grid.cells_generation, 1,
            "an empty diff must not bump the cells generation"
        );
        assert_eq!(grid.cursor_generation, 2);
    }

    #[test]
    fn apply_snapshot_evicts_below_scrollback_base() {
        // The clear-then-resize leak: the client holds scrollback, then a fresh
        // snapshot arrives with an EMPTY tail but an advanced base (history was
        // wiped). evict_before must drop the stale lines despite the empty tail.
        let mut grid = CellGrid::new(24, 80);
        push_scrollback(&mut grid, vec![line("a"), line("b"), line("c")]);
        assert_eq!(grid.scrollback_len(), 3);

        let mut snap = snapshot(24, 80);
        snap.history_total = 3;
        snap.scrollback_base = 3; // daemon wiped everything below index 3
        grid.apply_snapshot(snap);
        assert_eq!(
            grid.scrollback_len(),
            0,
            "stale lines dropped even though the snapshot tail is empty"
        );
        assert_eq!(grid.scrollback().history_total(), 3);
    }

    #[test]
    fn apply_diff_flags_pending_history_gap() {
        let mut grid = CellGrid::new(24, 80);
        let diff = TerminalDiff {
            ops: vec![],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 5,
            scrollback_reset: None,
        };
        grid.apply_diff(diff);
        assert_eq!(grid.pending_history_gap(), Some((0, 5)));

        grid.apply_scrollback_append(
            0,
            vec![line("a"), line("b"), line("c"), line("d"), line("e")]
                .into_iter()
                .map(Into::into)
                .collect(),
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

    fn pos(row: usize, col: usize) -> GridPos {
        GridPos { row, col }
    }

    fn select(grid: &mut CellGrid, anchor: GridPos, end: GridPos) {
        grid.set_selection(Some(Selection {
            anchor,
            end,
            mode: SelectionMode::Normal,
        }));
    }

    #[test]
    fn visible_to_abs_live_view() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(&mut grid, vec![line("aaaa"), line("bbbb")]);
        // scroll_offset == 0: visible row vr maps to live row sb_len + vr.
        assert_eq!(grid.visible_to_abs(0, 0), pos(2, 0));
        assert_eq!(grid.visible_to_abs(2, 3), pos(4, 3));
        // Clamps out-of-range vr/vc to the grid.
        assert_eq!(grid.visible_to_abs(99, 99), pos(4, 3));
    }

    #[test]
    fn visible_to_abs_scrolled_plain_lines() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(
            &mut grid,
            vec![line("00"), line("11"), line("22"), line("33")],
        );
        grid.scroll_up(2);
        assert_eq!(grid.scroll_offset(), 2);
        // Top two rows show scrollback lines 2 and 3; third row is live row 0.
        assert_eq!(grid.visible_to_abs(0, 0), pos(2, 0));
        assert_eq!(grid.visible_to_abs(1, 1), pos(3, 1));
        assert_eq!(grid.visible_to_abs(2, 0), pos(4, 0));
    }

    #[test]
    fn visible_to_abs_scrolled_wrapped_line() {
        let mut grid = CellGrid::new(3, 4);
        // "ABCDEFGH" wraps into 2 display rows at cols=4; "tail" is 1 row.
        push_scrollback(&mut grid, vec![line("ABCDEFGH"), line("tail")]);
        grid.scroll_up(3);
        assert_eq!(grid.scroll_offset(), 3);
        // vr0/vr1 are the two slices of the wide logical line (index 0); the
        // column offset accumulates across the wrap (base 0, then base 4).
        assert_eq!(grid.visible_to_abs(0, 2), pos(0, 2));
        assert_eq!(grid.visible_to_abs(1, 1), pos(0, 5));
        // vr2 is the "tail" logical line (index 1).
        assert_eq!(grid.visible_to_abs(2, 3), pos(1, 3));
    }

    #[test]
    fn visible_selection_spans_across_scrollback_and_live() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(
            &mut grid,
            vec![line("0000"), line("1111"), line("2222"), line("3333")],
        );
        grid.scroll_up(2);
        // Selection from scrollback line 2 col 1 into live row 0 col 2.
        select(&mut grid, pos(2, 1), pos(4, 2));
        assert_eq!(
            grid.visible_selection_spans(),
            vec![(0, 1, 3), (1, 0, 3), (2, 0, 2)]
        );
    }

    #[test]
    fn visible_selection_spans_within_wrapped_line() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(&mut grid, vec![line("ABCDEFGH"), line("tail")]);
        grid.scroll_up(3);
        // Select columns 2..=6 of the wide logical line; spans split per slice.
        select(&mut grid, pos(0, 2), pos(0, 6));
        assert_eq!(grid.visible_selection_spans(), vec![(0, 2, 3), (1, 0, 2)]);
    }

    #[test]
    fn visible_selection_spans_empty_without_selection() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(&mut grid, vec![line("0000")]);
        assert!(grid.visible_selection_spans().is_empty());
        // Degenerate (anchor == end) selections produce no spans.
        select(&mut grid, pos(1, 0), pos(1, 0));
        assert!(grid.visible_selection_spans().is_empty());
    }

    #[test]
    fn selected_text_copies_full_wrapped_scrollback_lines() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(&mut grid, vec![line("ABCDEFGH"), line("WXYZ")]);
        // A non-terminal scrollback row copies its whole logical line, not just
        // the viewport width.
        select(&mut grid, pos(0, 0), pos(1, 3));
        assert_eq!(grid.selected_text().as_deref(), Some("ABCDEFGH\nWXYZ"));
    }

    #[test]
    fn selected_text_none_for_degenerate_selection() {
        let mut grid = CellGrid::new(3, 4);
        push_scrollback(&mut grid, vec![line("abcd")]);
        select(&mut grid, pos(0, 1), pos(0, 1));
        assert_eq!(grid.selected_text(), None);
    }

    #[test]
    fn selected_text_trims_trailing_blanks_per_line() {
        let mut grid = CellGrid::new(3, 8);
        // "hi" padded with default blanks, then a second line.
        let mut padded = line("hi");
        padded.extend(vec![CellState::default(); 6]);
        push_scrollback(&mut grid, vec![padded, line("bye")]);
        select(&mut grid, pos(0, 0), pos(1, 2));
        assert_eq!(grid.selected_text().as_deref(), Some("hi\nbye"));
    }

    #[test]
    fn find_word_boundaries_live_grid_word() {
        let mut grid = CellGrid::new(1, 16);
        grid.apply_snapshot(GridSnapshot {
            rows: 1,
            cols: 16,
            cells: line("  foo_bar  baz  "),
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        });
        // Click inside "foo_bar" (cols 2..=8) selects the whole word.
        let (s, e) = grid.find_word_boundaries(pos(0, 4));
        assert_eq!((s, e), (pos(0, 2), pos(0, 8)));
        // Click on a blank selects just that cell.
        assert_eq!(grid.find_word_boundaries(pos(0, 0)), (pos(0, 0), pos(0, 0)));
    }

    #[test]
    fn find_word_boundaries_spans_past_viewport_in_wide_scrollback() {
        let mut grid = CellGrid::new(3, 4);
        // A 10-char word in a scrollback line far wider than the 4-col viewport.
        push_scrollback(&mut grid, vec![line("wxyzWXYZ12"), line("tail")]);
        // Click mid-word, past the viewport width: the whole word is selected,
        // not truncated at col 3.
        let (s, e) = grid.find_word_boundaries(pos(0, 6));
        assert_eq!((s, e), (pos(0, 0), pos(0, 9)));
    }
}
