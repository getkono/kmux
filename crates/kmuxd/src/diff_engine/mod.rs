mod compute;
mod mirror;

pub use mirror::ScrollbackMirror;

use kmux_protocol::messages::{CellState, CursorState, GridSnapshot, TermModes};

use crate::backend::{BackendSize, TerminalBackend};

/// Maximum number of scrollback lines kept in the daemon's per-pane mirror.
///
/// When the mirror is full, the oldest line is evicted and `base_index` is
/// bumped. Clients that ask for indices below `base_index` receive an empty
/// or truncated response -- at that point the line is gone for good.
pub const MIRROR_CAPACITY: usize = 10_000;

/// Number of tail lines included in `GridSnapshot::scrollback_tail` so a
/// reattaching client can render scrollback immediately without a round-trip
/// for `FetchHistory`.
pub const SNAPSHOT_TAIL_LINES: usize = 500;

/// Result of a diff computation, distinguishing between cell changes,
/// cursor-only changes, and no changes at all.
///
/// Scrollback lines appended during this frame travel alongside `CellDiff`
/// in `scrollback_lines`; the relay emits them as a separate
/// `ScrollbackAppend` message and never bundles them into `TerminalDiff`.
#[derive(Debug)]
pub enum DiffResult {
    /// At least one cell changed, or scrollback was appended (possibly with
    /// no viewport changes), optionally combined with cursor/mode changes.
    CellDiff {
        diff: kmux_protocol::messages::TerminalDiff,
        /// Lines newly appended to the mirror during this frame, oldest
        /// first. Empty if only viewport cells changed.
        scrollback_lines: Vec<Vec<kmux_protocol::messages::CellState>>,
    },
    /// No cells changed, but cursor position or terminal modes changed.
    CursorOnly {
        cursor: CursorState,
        modes: TermModes,
        /// Mirror's `history_total` as of this diff. Carried so the relay
        /// can stamp replay-placeholder `TerminalDiff`s with a monotonic
        /// value without reaching back into the engine.
        history_total: u64,
    },
    /// Nothing changed since the last diff.
    None,
}

/// Generic diff engine that wraps any [`TerminalBackend`] and computes
/// frame-to-frame cell diffs.
///
/// Maintains two cell buffers (previous and current) and detects changes
/// by comparing them after each `feed()` + `compute_diff()` cycle.
pub struct DiffEngine<B: TerminalBackend> {
    pub(super) backend: B,
    pub(super) prev_cells: Vec<CellState>,
    /// Reusable scratch buffer -- avoids allocation per `compute_diff()` call.
    pub(super) current_cells: Vec<CellState>,
    pub(super) prev_cursor: CursorState,
    pub(super) prev_modes: TermModes,
    pub(super) rows: u16,
    pub(super) cols: u16,
    /// Backend history size at the end of the previous `compute_diff()` call.
    pub(super) prev_history_size: usize,
    /// Saved main-screen history size when entering the alternate screen buffer.
    /// Used to avoid re-sending the entire scrollback when exiting alt screen.
    pub(super) saved_main_history_size: Option<usize>,
    /// Bounded mirror of scrollback lines with absolute `u64` indices,
    /// independent of the backend's own scrollback. Survives resize reflows
    /// and alt-screen transitions, so reattaching clients can render
    /// scrollback without waiting on the backend's volatile state.
    pub(super) mirror: ScrollbackMirror,
}

impl<B: TerminalBackend> DiffEngine<B> {
    pub fn new(backend: B) -> Self {
        let sz = backend.size();
        let rows = sz.rows;
        let cols = sz.cols;
        let n = rows as usize * cols as usize;
        let blank = CellState::default();
        let prev_history_size = backend.history_size();
        Self {
            backend,
            prev_cells: vec![blank; n],
            current_cells: vec![blank; n],
            prev_cursor: CursorState::default(),
            prev_modes: TermModes::EMPTY,
            rows,
            cols,
            prev_history_size,
            saved_main_history_size: None,
            mirror: ScrollbackMirror::new(MIRROR_CAPACITY),
        }
    }

    /// Feed raw PTY output bytes through the VTE parser.
    pub fn feed(&mut self, data: &[u8]) {
        self.backend.feed(data);
    }

    /// Construct a `DiffEngine` with its previous-frame state seeded from a
    /// [`GridSnapshot`].
    ///
    /// Initialises `prev_cells` to match the snapshot so the next
    /// `compute_diff()` call produces no spurious full-screen diff.
    #[cfg(test)]
    pub fn from_snapshot(backend: B, snapshot: &GridSnapshot) -> Self {
        let rows = snapshot.rows;
        let cols = snapshot.cols;
        let n = rows as usize * cols as usize;

        // Pad or truncate to exactly rows*cols (handles snapshot/backend size mismatches).
        let mut prev_cells = vec![CellState::default(); n];
        let copy_len = snapshot.cells.len().min(n);
        prev_cells[..copy_len].copy_from_slice(&snapshot.cells[..copy_len]);

        let prev_history_size = backend.history_size();

        Self {
            backend,
            prev_cells,
            current_cells: vec![CellState::default(); n],
            prev_cursor: snapshot.cursor,
            prev_modes: snapshot.modes,
            rows,
            cols,
            prev_history_size,
            saved_main_history_size: None,
            mirror: ScrollbackMirror::new(MIRROR_CAPACITY),
        }
    }

    /// Absolute number of lines ever scrolled off the top of this pane, as
    /// tracked by the mirror. Monotonically non-decreasing across resizes.
    pub fn history_total(&self) -> u64 {
        self.mirror.history_total()
    }

    /// Fetch up to `count` scrollback lines from the mirror starting at the
    /// given absolute index. Returns `(first_index, lines)` where
    /// `first_index >= start` (clamped to `base_index` if `start` is older).
    pub fn mirror_range(&self, start: u64, count: u32) -> (u64, Vec<Vec<CellState>>) {
        self.mirror.range(start, count)
    }

    /// Number of lines currently in the backend's scrollback history.
    pub fn history_size(&self) -> usize {
        self.backend.history_size()
    }

    /// Read scrollback history lines from the backend.
    ///
    /// `start` is the oldest-first index; `count` is the number of lines to
    /// return. Each returned line has `self.cols` cells.
    pub fn read_history_lines(&self, start: usize, count: usize) -> Vec<Vec<CellState>> {
        self.backend
            .read_history_lines(start, count, self.cols as usize)
    }

    /// Current cursor state from the backend.
    #[cfg(test)]
    pub fn cursor(&self) -> CursorState {
        self.backend.cursor()
    }

    /// Current terminal modes from the backend.
    pub fn modes(&self) -> TermModes {
        self.backend.modes()
    }

    /// Encode a structured key event into terminal escape bytes using the
    /// backend's live mode state (DECCKM, kitty kbd flags, modifyOtherKeys, …).
    pub fn encode_key_event(&self, event: &kmux_protocol::messages::KeyEvent) -> Vec<u8> {
        self.backend.encode_key_event(event)
    }

    /// Take a full grid snapshot (for initial attach or post-resize).
    ///
    /// Includes a tail slice of the mirror (up to [`SNAPSHOT_TAIL_LINES`])
    /// and the absolute `history_total`, so reattaching clients can render
    /// scrollback immediately without an extra `FetchHistory` round-trip.
    pub fn snapshot(&self) -> GridSnapshot {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        let blank = CellState::default();
        let mut cells = vec![blank; rows * cols];
        let (cursor, modes) = self.backend.fill_cells_and_cursor(&mut cells);

        let scrollback_tail = self.mirror.tail(SNAPSHOT_TAIL_LINES);
        let history_total = self.mirror.history_total();

        GridSnapshot {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor,
            modes,
            history_total,
            scrollback_tail,
        }
    }

    /// Resize the terminal. Resets viewport comparison state so the next
    /// diff is full-grid, but preserves the scrollback mirror -- any lines
    /// the backend still carries beyond our last-seen head are appended
    /// before we forget them.
    pub fn resize(&mut self, size: BackendSize) {
        // Drain any still-visible scrollback the backend holds beyond what we
        // last mirrored, BEFORE resizing (the backend may drop or reflow
        // lines during `resize()`).
        let current_history_size = self.backend.history_size();
        if !self.backend.is_alt_screen() && current_history_size > self.prev_history_size {
            let new_count = current_history_size - self.prev_history_size;
            let start = self.prev_history_size;
            let lines = self
                .backend
                .read_history_lines(start, new_count, self.cols as usize);
            if !lines.is_empty() {
                self.mirror.append(lines);
            }
        }

        self.rows = size.rows;
        self.cols = size.cols;
        self.backend.resize(size);
        let n = size.rows as usize * size.cols as usize;
        self.prev_cells = vec![CellState::default(); n];
        self.current_cells = vec![CellState::default(); n];
        self.prev_cursor = CursorState::default();
        self.prev_modes = TermModes::EMPTY;
        // NOTE: do NOT reset `prev_history_size` to the post-resize backend
        // value; that would mask any lines the backend evicted during reflow.
        // The next `compute_diff()` re-reads `backend.history_size()` and
        // appends the delta to the mirror.
        self.prev_history_size = self.backend.history_size();
        self.saved_main_history_size = None;
    }
}
