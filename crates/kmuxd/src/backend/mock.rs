use kmux_protocol::messages::{CellState, CursorState, TermModes};

use super::{BackendConfig, BackendSize, TerminalBackend};

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
    pub alt_screen: bool,
    pub history_len: usize,
    pub history_lines: Vec<Vec<CellState>>,
    size: BackendSize,
}

impl MockBackend {
    /// Convenience constructor for tests that don't need pixel dims or capabilities.
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
            size: BackendSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
        }
    }
}

impl TerminalBackend for MockBackend {
    fn new(cfg: BackendConfig) -> Self {
        let size = cfg.size;
        let n = size.rows as usize * size.cols as usize;
        Self {
            cells: vec![CellState::default(); n],
            cursor_state: CursorState::default(),
            mode_flags: TermModes::EMPTY,
            fed_bytes: Vec::new(),
            alt_screen: false,
            history_len: 0,
            history_lines: Vec::new(),
            size,
        }
    }

    fn name() -> &'static str
    where
        Self: Sized,
    {
        "mock"
    }

    fn feed(&mut self, data: &[u8]) {
        self.fed_bytes.extend_from_slice(data);
    }

    fn size(&self) -> BackendSize {
        self.size
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

    fn resize(&mut self, size: BackendSize) {
        self.size = size;
        let n = size.rows as usize * size.cols as usize;
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
