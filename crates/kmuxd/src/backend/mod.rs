use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kmux_protocol::messages::{CellState, CursorState, TermModes, TermSize};

#[cfg(feature = "backend-ghostty")]
pub mod ghostty;
#[cfg(test)]
pub mod mock;
#[cfg(feature = "backend-wezterm")]
pub mod wezterm;

/// Default maximum scrollback lines retained by the terminal emulator.
pub const DEFAULT_SCROLLBACK: usize = 50_000;

/// Physical size of the terminal including optional pixel dimensions.
///
/// `pixel_width` and `pixel_height` are `0` when the platform does not expose
/// them.  Backends that support graphics protocols (sixel, kitty-image) should
/// use these for image scaling; backends that only do cell rendering may ignore
/// them safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendSize {
    pub rows: u16,
    pub cols: u16,
    /// Total drawable width in physical pixels; `0` = unknown.
    pub pixel_width: u16,
    /// Total drawable height in physical pixels; `0` = unknown.
    pub pixel_height: u16,
}

impl From<TermSize> for BackendSize {
    fn from(t: TermSize) -> Self {
        Self {
            rows: t.rows,
            cols: t.cols,
            pixel_width: t.pixel_width,
            pixel_height: t.pixel_height,
        }
    }
}

impl From<BackendSize> for TermSize {
    fn from(b: BackendSize) -> Self {
        Self {
            rows: b.rows,
            cols: b.cols,
            pixel_width: b.pixel_width,
            pixel_height: b.pixel_height,
        }
    }
}

/// Live-updateable feature toggles shared with the pane relay.
///
/// The daemon writes to these atomics on every client attach/detach; the
/// backend reads them on every relevant escape-sequence handler without
/// rebuilding the terminal state.
///
/// Reader coverage depends on which backend feature is active: wezterm
/// consults them via `KmuxTerminalConfig`; the current ghostty-vt wrapper
/// parses unconditionally and never reads these. The fields are therefore
/// marked `#[allow(dead_code)]` at the `backend-ghostty`-only cfg.
#[cfg_attr(
    all(feature = "backend-ghostty", not(feature = "backend-wezterm")),
    allow(dead_code)
)]
pub struct CapabilityHandles {
    pub kitty_graphics: Arc<AtomicBool>,
    pub kitty_keyboard: Arc<AtomicBool>,
}

/// Backend-to-host event channel.
///
/// Implementations **MUST NOT block** — this trait is called from the VT
/// parser loop.  Any I/O should be pushed to an unbounded `mpsc` channel and
/// drained from a separate task.
pub trait BackendEventSink: Send + Sync + 'static {
    fn on_title(&self, _title: &str) {}
    fn on_bell(&self) {}
    // Seams for future backends (e.g. GhosttyBackend): called when the
    // backend processes an OSC 52 copy or hyperlink sequence.
    #[allow(dead_code)]
    fn on_osc52_copy(&self, _selection: &str, _base64_data: &str) {}
    #[allow(dead_code)]
    fn on_hyperlink(&self, _id: Option<&str>, _uri: &str) {}
}

/// A no-op event sink used when the host does not need backend events.
pub struct NullEventSink;
impl BackendEventSink for NullEventSink {}

/// Configuration passed to [`TerminalBackend::new`].
pub struct BackendConfig {
    pub size: BackendSize,
    /// Live kitty-graphics/keyboard toggles. Read by wezterm; ghostty-vt
    /// ignores them today (parse-unconditionally model).
    #[cfg_attr(
        all(feature = "backend-ghostty", not(feature = "backend-wezterm")),
        allow(dead_code)
    )]
    pub capabilities: CapabilityHandles,
    /// Event sink for title/bell/OSC callbacks from the terminal emulator.
    pub events: Arc<dyn BackendEventSink>,
    /// Maximum number of lines to keep in the scrollback buffer.
    pub scrollback: usize,
}

/// Abstraction over a VT emulator backend.
///
/// Backends parse VTE bytes, maintain grid state, and resolve colors to RGB.
/// The shared [`DiffEngine`](crate::diff_engine::DiffEngine) handles
/// frame-to-frame diffing, clear detection, and buffer management.
///
/// # Object safety
///
/// This trait is intentionally **not** object-safe: `new` and `name` have
/// `where Self: Sized` bounds.  This preserves static dispatch through
/// `DiffEngine<B>` which is the only client of this trait today.  If runtime
/// backend selection is ever needed, introduce an erased wrapper — do not
/// remove the `Sized` bounds.
pub trait TerminalBackend: Send + 'static {
    /// Construct a new backend from a [`BackendConfig`].
    fn new(cfg: BackendConfig) -> Self
    where
        Self: Sized;

    /// Human-readable name of this backend (e.g. `"wezterm"`, `"ghostty"`).
    fn name() -> &'static str
    where
        Self: Sized;

    /// Feed raw PTY output bytes through the VTE parser.
    fn feed(&mut self, data: &[u8]);

    /// Current grid dimensions.
    fn size(&self) -> BackendSize;

    /// Populate `out` with the current grid state in row-major order.
    ///
    /// `out` is pre-sized to `rows * cols` and pre-filled with defaults.
    fn fill_cells(&self, out: &mut [CellState]);

    /// Current cursor position and shape.
    fn cursor(&self) -> CursorState;

    /// Current terminal mode flags.
    fn modes(&self) -> TermModes;

    /// Resize the underlying terminal emulator.
    fn resize(&mut self, size: BackendSize);

    /// Populate cells AND return cursor+modes in a single pass.
    ///
    /// Backends where `fill_cells()` and `cursor()` share expensive
    /// intermediate state should override this to avoid redundant work.
    /// The default calls each method individually.
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

/// Marker trait for future runtime backend selection.
///
/// Reserved: when vtable-erased backend switching is worth the cost, a factory
/// registry will use this trait to construct backends by name.  Until then,
/// selection is static via `ActiveBackend` in `term_state.rs`.
#[allow(dead_code)]
pub trait BackendFactory: Send + Sync + 'static {
    fn name(&self) -> &'static str;
}
