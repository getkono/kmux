use smux_protocol::messages::{CellState, CursorState, TermModes};

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
}
