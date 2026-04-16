mod compute;

use kmux_protocol::messages::{CellState, CursorState, GridSnapshot, TermModes};

use crate::backend::TerminalBackend;

/// Result of a diff computation, distinguishing between cell changes,
/// cursor-only changes, and no changes at all.
#[derive(Debug)]
pub enum DiffResult {
    /// At least one cell changed (may also include cursor/mode changes).
    CellDiff(kmux_protocol::messages::TerminalDiff),
    /// No cells changed, but cursor position or terminal modes changed.
    CursorOnly {
        cursor: CursorState,
        modes: TermModes,
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
}

impl<B: TerminalBackend> DiffEngine<B> {
    pub fn new(backend: B) -> Self {
        let (rows, cols) = backend.size();
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
        }
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

    /// Take a full grid snapshot (for initial attach or post-resize).
    pub fn snapshot(&self) -> GridSnapshot {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        let blank = CellState::default();
        let mut cells = vec![blank; rows * cols];
        let (cursor, modes) = self.backend.fill_cells_and_cursor(&mut cells);

        GridSnapshot {
            rows: self.rows,
            cols: self.cols,
            cells,
            cursor,
            modes,
        }
    }

    /// Resize the terminal. Resets `prev_cells` so the next diff is full-grid.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        self.backend.resize(rows, cols);
        let n = rows as usize * cols as usize;
        self.prev_cells = vec![CellState::default(); n];
        self.current_cells = vec![CellState::default(); n];
        self.prev_cursor = CursorState::default();
        self.prev_modes = TermModes::EMPTY;
        self.prev_history_size = self.backend.history_size();
        self.saved_main_history_size = None;
    }
}
