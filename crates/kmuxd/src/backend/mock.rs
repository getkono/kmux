use kmux_protocol::messages::{CellState, CursorState, TermModes};

use super::TerminalBackend;

/// A mock terminal backend for testing the diff engine in isolation.
///
/// `feed()` records input bytes for assertion but does not parse VTE.
/// Set `cells`, `cursor_state`, and `mode_flags` directly to control
/// what `DiffEngine` sees on the next `compute_diff()` call.
#[allow(dead_code)]
pub struct MockBackend {
    pub cells: Vec<CellState>,
    pub cursor_state: CursorState,
    pub mode_flags: TermModes,
    pub fed_bytes: Vec<u8>,
    pub alt_screen: bool,
    pub history_len: usize,
    pub history_lines: Vec<Vec<CellState>>,
    rows: u16,
    cols: u16,
}

#[allow(dead_code)]
impl MockBackend {
    pub fn new(rows: u16, cols: u16) -> Self {
        let n = rows as usize * cols as usize;
        Self {
            cells: vec![CellState::default(); n],
            cursor_state: CursorState::default(),
            mode_flags: TermModes::EMPTY,
            fed_bytes: Vec::new(),
            alt_screen: false,
            history_len: 0,
            history_lines: Vec::new(),
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

    fn is_alt_screen(&self) -> bool {
        self.alt_screen
    }

    fn history_size(&self) -> usize {
        self.history_len
    }

    fn read_history_lines(&self, start: usize, count: usize, _cols: usize) -> Vec<Vec<CellState>> {
        self.history_lines
            .iter()
            .skip(start)
            .take(count)
            .cloned()
            .collect()
    }
}
