//! Authoritative terminal content: cells, cursor, modes, and scrollback.
//!
//! [`GridContent`] is the apply-mutated half of a pane's state — everything the
//! server's diff stream drives. It carries no view-local state (`scroll_offset`,
//! `selection`); those live in [`GridView`](super::view::GridView). Splitting the
//! two lets the content be applied on a worker thread and published to the UI as
//! an immutable snapshot (issue #182, §1) while the UI keeps mutating its view
//! every frame with no cross-thread round-trip.
//!
//! Apply methods are pure content mutations: any view-state consequence
//! (snap-to-bottom, selection reconciliation after scrollback eviction) is
//! returned as an [`ApplyEffect`] for the view owner to apply, never reached
//! into directly. The daemon reuses `GridContent` as its headless pane mirror,
//! where the returned effects are simply ignored (it has no view).

use kmux_protocol::messages::{
    CellState, CursorState, DiffOp, GridSnapshot, ScrollbackLine, TermModes, TerminalDiff,
};

use super::scrollback::ScrollbackBuffer;
use super::selection::GridPos;
use super::{display_rows_for_line, effective_line_len};

/// View-state consequences of an apply, for the [`GridView`](super::view::GridView)
/// owner to apply. Empty (`Default`) when an apply touched only content.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApplyEffect {
    /// Snap the viewport to the bottom and clear any selection — a snapshot,
    /// clear, resize, or scrollback-reset diff invalidated the view's anchors.
    pub reset_view: bool,
    /// A scrollback append shifted absolute line indices; the view must
    /// reconcile its selection rows. `None` when no append happened.
    pub scrollback_fixup: Option<ScrollbackFixup>,
}

/// How a scrollback append affects view-local selection rows.
#[derive(Debug, Clone, Copy)]
pub enum ScrollbackFixup {
    /// The append had a gap/overlap and the buffer was reset; drop selection.
    Cleared,
    /// `evicted` lines fell off the front and `net` lines were net-added; the
    /// view shifts selection rows by `net - evicted`, dropping it if the anchor
    /// fell off the front.
    Shifted { evicted: usize, net: usize },
}

/// Authoritative, apply-mutated terminal content. See the module docs.
#[derive(Clone)]
pub struct GridContent {
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
    /// Highest `history_total` the server has reported. When greater than
    /// `scrollback.history_total()`, there are scrollback lines the client has
    /// not yet received — the session manager will issue `FetchHistory` to
    /// fill the gap. Cleared when the gap closes.
    pending_history_total: Option<u64>,
    /// Per-row generation stamp (length `rows`): the `cells_generation` at which
    /// each live row last changed. A renderer that cached the scene at
    /// generation `g` need only rebuild rows whose stamp is `> g` (issue #182,
    /// §3). Carried through the off-thread publish snapshot for free.
    row_gens: Vec<u64>,
}

impl GridContent {
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
            pending_history_total: None,
            row_gens: vec![0; rows],
        }
    }

    /// Stamp every live row as changed at the current `cells_generation`. Used
    /// when the whole viewport is rewritten (snapshot, clear, resize).
    fn stamp_all_rows(&mut self) {
        let stamp = self.cells_generation;
        self.row_gens.clear();
        self.row_gens.resize(self.rows, stamp);
    }

    /// The `cells_generation` at which live `row` last changed (0 if never, or
    /// out of range). A renderer reuses a row's cached geometry while this is
    /// unchanged. See `row_gens`.
    pub fn row_generation(&self, row: usize) -> u64 {
        self.row_gens.get(row).copied().unwrap_or(0)
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
    pub fn apply_snapshot(&mut self, snapshot: GridSnapshot) -> ApplyEffect {
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
        self.cells_generation += 1;
        self.cursor_generation += 1;
        self.stamp_all_rows();
        ApplyEffect {
            reset_view: true,
            ..ApplyEffect::default()
        }
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
    /// Re-serialises a live grid so a fresh consumer can reconstruct the exact
    /// view with no upstream round-trip (issue #121: the federation daemon mints
    /// a snapshot for a newly-attaching GUI from its authoritative pane mirror).
    /// The full held scrollback travels as `scrollback_tail` so the consumer
    /// renders history immediately. View-local `scroll_offset` / `selection` are
    /// not part of the wire snapshot.
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
    /// `FetchHistory` request. A scrollback-reset diff returns `reset_view`.
    pub fn apply_diff(&mut self, diff: TerminalDiff) -> ApplyEffect {
        // A scrollback wipe (`clear`'s CSI 3J, `RIS`) must drop history BEFORE
        // the gap check below, so any surviving lines arrive cleanly via the
        // out-of-band append (which the relay orders after this diff) and no
        // spurious gap is recorded. `history_total` stays monotonic across the
        // wipe, so `reset_to(base)` re-anchors at the daemon's new oldest index.
        let scrollback_reset = diff.scrollback_reset.is_some();
        if let Some(base) = diff.scrollback_reset {
            self.scrollback.reset_to(base);
            self.pending_history_total = None;
        }

        if diff.history_total > self.scrollback.history_total() {
            self.pending_history_total = Some(diff.history_total);
        }

        let has_cell_ops = !diff.ops.is_empty();
        // Rows touched this diff, stamped with the post-bump generation below so
        // a renderer can rebuild only what changed (issue #182, §3).
        let mut changed_rows: Vec<u16> = Vec::new();
        let mut clear_all = false;
        for op in diff.ops {
            match op {
                DiffOp::Cell { row, col, cell } => {
                    let idx = row as usize * self.cols + col as usize;
                    if idx < self.cells.len() {
                        self.cells[idx] = cell;
                    }
                    changed_rows.push(row);
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
                    changed_rows.push(row);
                }
                DiffOp::Clear => {
                    self.cells.fill(CellState::default());
                    clear_all = true;
                }
            }
        }
        self.cursor = diff.cursor;
        self.modes = diff.modes;
        if has_cell_ops || scrollback_reset {
            self.cells_generation += 1;
        }
        self.cursor_generation += 1;
        if has_cell_ops {
            let stamp = self.cells_generation;
            if clear_all {
                self.row_gens.iter_mut().for_each(|g| *g = stamp);
            }
            for row in changed_rows {
                if let Some(g) = self.row_gens.get_mut(row as usize) {
                    *g = stamp;
                }
            }
        }
        ApplyEffect {
            reset_view: scrollback_reset,
            ..ApplyEffect::default()
        }
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
    /// (driven by the session manager) will reseed it. The returned
    /// [`ScrollbackFixup`] tells the view how to reconcile its selection rows.
    pub fn apply_scrollback_append(
        &mut self,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
    ) -> ApplyEffect {
        if lines.is_empty() {
            return ApplyEffect::default();
        }
        let new_count = lines.len();
        let old_len = self.scrollback.len();
        let ok = self.scrollback.append_with_index(first_index, lines);
        let fixup = if ok {
            let new_len = self.scrollback.len();
            let evicted = (old_len + new_count).saturating_sub(new_len);
            ScrollbackFixup::Shifted {
                evicted,
                net: new_count - evicted,
            }
        } else {
            self.scrollback.clear();
            ScrollbackFixup::Cleared
        };
        self.maybe_clear_history_gap();
        self.cells_generation += 1;
        ApplyEffect {
            scrollback_fixup: Some(fixup),
            ..ApplyEffect::default()
        }
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
    pub fn clear(&mut self) -> ApplyEffect {
        self.cells.fill(CellState::default());
        self.cursor = CursorState::default();
        self.modes = TermModes::EMPTY;
        self.scrollback.clear();
        self.pending_history_total = None;
        self.cells_generation += 1;
        self.cursor_generation += 1;
        self.stamp_all_rows();
        ApplyEffect {
            reset_view: true,
            ..ApplyEffect::default()
        }
    }

    /// Resize the grid (server will send a fresh snapshot after resize).
    ///
    /// Scrollback is intentionally preserved across resize. The viewport is
    /// zeroed and the incoming snapshot will populate it; previously-captured
    /// scrollback lines remain at the width they were captured and are
    /// wrap-rendered to the new viewport width.
    pub fn resize(&mut self, rows: u16, cols: u16) -> ApplyEffect {
        self.rows = rows as usize;
        self.cols = cols as usize;
        self.cells = vec![CellState::default(); self.rows * self.cols];
        self.cells_generation += 1;
        self.cursor_generation += 1;
        self.stamp_all_rows();
        ApplyEffect {
            reset_view: true,
            ..ApplyEffect::default()
        }
    }

    /// Generation counter that changes on every update (used to detect changes).
    pub fn generation(&self) -> u64 {
        self.cursor_generation
    }

    /// Generation counter that changes only when cells change (used for cache
    /// invalidation). The facade folds in a view generation so scrolling also
    /// invalidates the renderer cache.
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

    /// Number of lines in scrollback history.
    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
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
        let ch = self.cell_at(pos).map_or(' ', |c| c.c);
        let is_word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~');

        if is_word(ch) {
            let mut start_col = pos.col;
            while start_col > 0 {
                let prev = GridPos {
                    row: pos.row,
                    col: start_col - 1,
                };
                if self.cell_at(prev).is_some_and(|c| is_word(c.c)) {
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
                    .map_or(0, |l| effective_line_len(l).saturating_sub(1))
            } else {
                self.cols.saturating_sub(1)
            };
            let mut end_col = pos.col;
            while end_col < max_col {
                let next = GridPos {
                    row: pos.row,
                    col: end_col + 1,
                };
                if self.cell_at(next).is_some_and(|c| is_word(c.c)) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::CursorState;

    fn cell_diff(row: u16, col: u16, ch: char) -> TerminalDiff {
        TerminalDiff {
            ops: vec![DiffOp::Cell {
                row,
                col,
                cell: CellState {
                    c: ch,
                    ..CellState::default()
                },
            }],
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        }
    }

    #[test]
    fn row_generation_stamps_only_changed_rows() {
        let mut g = GridContent::new(4, 8);
        // A diff touching row 2 stamps only row 2 with the new cells_generation.
        g.apply_diff(cell_diff(2, 1, 'X'));
        let stamp = g.cells_generation();
        assert_eq!(g.row_generation(2), stamp, "changed row carries the stamp");
        for r in [0, 1, 3] {
            assert_eq!(g.row_generation(r), 0, "untouched row {r} unchanged");
        }

        // A cursor-only diff (no cell ops) bumps no row generation.
        g.apply_diff(TerminalDiff {
            ops: vec![],
            cursor: CursorState {
                col: 3,
                ..CursorState::default()
            },
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        });
        assert_eq!(g.row_generation(2), stamp, "cursor-only leaves row stamps");

        // A full clear stamps every row at the new generation.
        g.clear();
        let after_clear = g.cells_generation();
        for r in 0..4 {
            assert_eq!(g.row_generation(r), after_clear, "clear stamps row {r}");
        }
    }

    #[test]
    fn resize_rebuilds_row_generations_to_new_height() {
        let mut g = GridContent::new(3, 4);
        g.resize(6, 8);
        let stamp = g.cells_generation();
        for r in 0..6 {
            assert_eq!(g.row_generation(r), stamp);
        }
        assert_eq!(g.row_generation(6), 0, "out-of-range row reads 0");
    }
}
