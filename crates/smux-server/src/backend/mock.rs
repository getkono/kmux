use smux_protocol::messages::{CellState, CursorState, TermModes};

use super::TerminalBackend;

/// A mock terminal backend for testing the diff engine in isolation.
///
/// `feed()` records input bytes for assertion but does not parse VTE.
/// Set `cells`, `cursor_state`, and `mode_flags` directly to control
/// what `DiffEngine` sees on the next `compute_diff()` call.
pub struct MockBackend {
    pub cells: Vec<CellState>,
    pub cursor_state: CursorState,
    pub mode_flags: TermModes,
    pub fed_bytes: Vec<u8>,
    rows: u16,
    cols: u16,
}

impl MockBackend {
    pub fn new(rows: u16, cols: u16) -> Self {
        let n = rows as usize * cols as usize;
        Self {
            cells: vec![CellState::default(); n],
            cursor_state: CursorState::default(),
            mode_flags: TermModes::EMPTY,
            fed_bytes: Vec::new(),
            rows,
            cols,
        }
    }
}

impl TerminalBackend for MockBackend {
    fn feed(&mut self, data: &[u8]) {
        self.fed_bytes.extend_from_slice(data);
    }

    fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    fn fill_cells(&self, out: &mut [CellState]) {
        let n = self.cells.len().min(out.len());
        out[..n].copy_from_slice(&self.cells[..n]);
    }

    fn cursor(&self) -> CursorState {
        self.cursor_state
    }

    fn modes(&self) -> TermModes {
        self.mode_flags
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
        let n = rows as usize * cols as usize;
        self.cells.resize(n, CellState::default());
    }
}
