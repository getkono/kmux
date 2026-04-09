use kmux_protocol::messages::{CellState, CursorState, TermModes};

#[cfg(feature = "backend-alacritty")]
pub mod alacritty;
pub mod mock;
#[cfg(feature = "backend-termwiz")]
pub mod termwiz;

/// Abstraction over a VT emulator backend.
///
/// Backends are responsible for: parsing VTE bytes, maintaining grid state,
/// and resolving colors to RGB. The shared [`DiffEngine`](crate::diff_engine::DiffEngine)
/// handles frame-to-frame diffing, clear detection, and buffer management.
pub trait TerminalBackend: Send + 'static {
    /// Feed raw PTY output bytes through the VTE parser.
    fn feed(&mut self, data: &[u8]);

    /// Current grid dimensions `(rows, cols)`.
    fn size(&self) -> (u16, u16);

    /// Populate `out` with the current grid state in row-major order.
    ///
    /// `out` is pre-sized to `rows * cols` and pre-filled with defaults.
    fn fill_cells(&self, out: &mut [CellState]);

    /// Current cursor position and shape.
    fn cursor(&self) -> CursorState;

    /// Current terminal mode flags.
    fn modes(&self) -> TermModes;

    /// Resize the underlying terminal emulator.
    fn resize(&mut self, rows: u16, cols: u16);

    /// Populate cells AND return cursor+modes in a single pass.
    ///
    /// Backends where `fill_cells()` and `cursor()` share expensive
    /// intermediate state (e.g. alacritty's `renderable_content()`)
    /// should override this to avoid redundant work. The default calls
    /// each method individually.
    fn fill_cells_and_cursor(&self, out: &mut [CellState]) -> (CursorState, TermModes) {
        self.fill_cells(out);
        (self.cursor(), self.modes())
    }

    /// Whether the terminal is currently on the alternate screen buffer.
    fn is_alt_screen(&self) -> bool {
        false
    }

    /// Number of lines currently in the scrollback history.
    fn history_size(&self) -> usize {
        0
    }

    /// Read `count` lines from the scrollback history starting at `start`.
    ///
    /// Index 0 is the oldest line in history. Each returned line is a
    /// `Vec<CellState>` of length `cols`.
    fn read_history_lines(
        &self,
        _start: usize,
        _count: usize,
        _cols: usize,
    ) -> Vec<Vec<CellState>> {
        vec![]
    }
}
