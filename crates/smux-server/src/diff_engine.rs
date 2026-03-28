use smux_protocol::messages::{
    CellState, CursorState, DiffOp, GridSnapshot, TermModes, TerminalDiff,
};

use crate::backend::TerminalBackend;

/// Generic diff engine that wraps any [`TerminalBackend`] and computes
/// frame-to-frame cell diffs.
///
/// Maintains two cell buffers (previous and current) and detects changes
/// by comparing them after each `feed()` + `compute_diff()` cycle.
pub struct DiffEngine<B: TerminalBackend> {
    backend: B,
    prev_cells: Vec<CellState>,
    /// Reusable scratch buffer -- avoids allocation per `compute_diff()` call.
    current_cells: Vec<CellState>,
    prev_cursor: CursorState,
    prev_modes: TermModes,
    rows: u16,
    cols: u16,
}

impl<B: TerminalBackend> DiffEngine<B> {
    pub fn new(backend: B) -> Self {
        let (rows, cols) = backend.size();
        let n = rows as usize * cols as usize;
        let blank = CellState::default();
        Self {
            backend,
            prev_cells: vec![blank; n],
            current_cells: vec![blank; n],
            prev_cursor: CursorState::default(),
            prev_modes: TermModes::EMPTY,
            rows,
            cols,
        }
    }

    /// Feed raw PTY output bytes through the VTE parser.
    pub fn feed(&mut self, data: &[u8]) {
        self.backend.feed(data);
    }

    /// Compute a diff between the current grid and `prev_cells`, then update
    /// `prev_cells` to match the current grid.
    pub fn compute_diff(&mut self) -> Option<TerminalDiff> {
        let rows = self.rows as usize;
        let cols = self.cols as usize;

        // Reset scratch buffer and populate from backend
        self.current_cells.fill(CellState::default());
        self.backend.fill_cells(&mut self.current_cells);

        // Compare all rows
        let mut ops = Vec::new();
        for r in 0..rows {
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

        // Detect full-screen clear: if all current cells are default and more
        // than half the screen changed, collapse into a single DiffOp::Clear.
        if !ops.is_empty() {
            let total = rows * cols;
            let changed: usize = ops
                .iter()
                .map(|op| match op {
                    DiffOp::Cell { .. } => 1,
                    DiffOp::Row { cells, .. } => cells.len(),
                    DiffOp::Clear => total,
                })
                .sum();
            let all_default = self.current_cells[..total]
                .iter()
                .all(|c| *c == CellState::default());
            if all_default && changed > total / 2 {
                ops = vec![DiffOp::Clear];
            }
        }

        // Swap buffers: current becomes prev for next frame
        std::mem::swap(&mut self.prev_cells, &mut self.current_cells);

        let cursor_state = self.backend.cursor();
        let modes = self.backend.modes();

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

        let blank = CellState::default();
        let mut cells = vec![blank; rows * cols];
        self.backend.fill_cells(&mut cells);

        let cursor_state = self.backend.cursor();
        let modes = self.backend.modes();

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
        self.backend.resize(rows, cols);
        let n = rows as usize * cols as usize;
        self.prev_cells = vec![CellState::default(); n];
        self.current_cells = vec![CellState::default(); n];
        self.prev_cursor = CursorState::default();
        self.prev_modes = TermModes::EMPTY;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mock::MockBackend;
    use smux_protocol::messages::CellColor;

    fn mock_engine(rows: u16, cols: u16) -> DiffEngine<MockBackend> {
        DiffEngine::new(MockBackend::new(rows, cols))
    }

    #[test]
    fn no_op_feed_produces_none() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial (all blank)
        engine.feed(b"");
        assert!(engine.compute_diff().is_none());
    }

    #[test]
    fn cell_change_produces_diff() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        // Mutate a cell in the mock backend
        engine.backend.cells[0] = CellState {
            c: 'X',
            fg: CellColor::new(0xff, 0x00, 0x00),
            bg: CellColor::new(0x28, 0x2c, 0x34),
            ..CellState::default()
        };
        let diff = engine.compute_diff().expect("expected Some diff");
        assert!(!diff.ops.is_empty());
    }

    #[test]
    fn cursor_move_without_cell_change_produces_some() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.cursor_state.col = 5;
        let diff = engine
            .compute_diff()
            .expect("cursor-only move should produce Some");
        assert!(diff.ops.is_empty(), "no cell changes expected");
        assert_eq!(diff.cursor.col, 5);
    }

    #[test]
    fn mode_change_without_cell_change_produces_some() {
        let mut engine = mock_engine(24, 80);
        let _ = engine.compute_diff(); // consume initial

        engine.backend.mode_flags = TermModes(TermModes::APP_CURSOR);
        let diff = engine
            .compute_diff()
            .expect("mode-only change should produce Some");
        assert!(diff.ops.is_empty(), "no cell changes expected");
        assert!(diff.modes.app_cursor());
    }

    #[test]
    fn clear_detection_when_all_default() {
        let mut engine = mock_engine(4, 4);
        // Fill all cells with non-default content
        for cell in &mut engine.backend.cells {
            *cell = CellState {
                c: 'X',
                ..CellState::default()
            };
        }
        let _ = engine.compute_diff(); // consume the fill

        // Now reset all to default (simulating CSI 2J)
        for cell in &mut engine.backend.cells {
            *cell = CellState::default();
        }
        let diff = engine.compute_diff().expect("expected Some diff");
        assert!(
            matches!(diff.ops.as_slice(), [DiffOp::Clear]),
            "expected DiffOp::Clear, got {:?}",
            diff.ops
        );
    }

    #[test]
    fn partial_clear_does_not_produce_clear_op() {
        let mut engine = mock_engine(4, 4);
        // Fill first row
        for c in 0..4 {
            engine.backend.cells[c] = CellState {
                c: 'X',
                ..CellState::default()
            };
        }
        let _ = engine.compute_diff();

        // Clear only first row
        for c in 0..4 {
            engine.backend.cells[c] = CellState::default();
        }
        let diff = engine.compute_diff().expect("expected Some diff");
        let has_clear = diff.ops.iter().any(|op| matches!(op, DiffOp::Clear));
        assert!(!has_clear, "partial erase should not produce DiffOp::Clear");
    }

    #[test]
    fn resize_resets_state() {
        let mut engine = mock_engine(24, 80);
        engine.backend.cells[0] = CellState {
            c: 'A',
            ..CellState::default()
        };
        let _ = engine.compute_diff();

        engine.resize(30, 100);
        assert_eq!(engine.rows, 30);
        assert_eq!(engine.cols, 100);
        // After resize, prev state is reset
        let _ = engine.compute_diff();
    }

    #[test]
    fn snapshot_returns_backend_state() {
        let mut engine = mock_engine(4, 4);
        engine.backend.cells[0] = CellState {
            c: 'A',
            ..CellState::default()
        };
        engine.backend.cursor_state.col = 1;

        let snap = engine.snapshot();
        assert_eq!(snap.rows, 4);
        assert_eq!(snap.cols, 4);
        assert_eq!(snap.cells[0].c, 'A');
        assert_eq!(snap.cursor.col, 1);
    }

    #[test]
    fn feed_records_bytes_in_mock() {
        let mut engine = mock_engine(4, 4);
        engine.feed(b"hello");
        assert_eq!(engine.backend.fed_bytes, b"hello");
    }
}
