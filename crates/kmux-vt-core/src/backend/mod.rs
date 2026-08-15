use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kmux_protocol::messages::{
    CellState, CursorState, KeyEvent, ScrollbackLine, TermModes, TermSize,
};

mod control_event;
pub mod ghostty;
#[cfg(test)]
pub mod mock;
mod vt_log;

pub use control_event::ControlEvent;
pub use vt_log::install_vt_log_forwarding;

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
/// The daemon writes to these atomics on every client attach/detach so that
/// the backend could gate sequence-parsing behaviour on the intersected
/// capabilities of attached clients.  libghostty-vt parses every supported
/// escape sequence unconditionally and therefore does not read these today;
/// the atomics are still populated because the kmux-protocol capability
/// negotiation contract requires them and a future backend (or wire-level
/// gating) may consume them.
#[allow(dead_code)]
pub struct CapabilityHandles {
    pub kitty_graphics: Arc<AtomicBool>,
    pub kitty_keyboard: Arc<AtomicBool>,
}

/// Backend-to-host event channel: the single dispatch point for every control
/// sequence kmux gives special treatment (issue #187).
///
/// Each consumer implements one method and `match`es on [`ControlEvent`], so the
/// whole of kmux's special VT behaviour is auditable from two `match`es (the
/// daemon relay and the isolated VT worker) plus the [`ControlEvent`] catalog.
///
/// Implementations **MUST NOT block** — this is called from the VT parser loop.
/// Any I/O should be pushed to an unbounded `mpsc` channel and drained from a
/// separate task.
pub trait BackendEventSink: Send + Sync + 'static {
    /// Handle one intercepted control sequence. The default drops it, so a sink
    /// only matches the variants it cares about.
    fn on_control_event(&self, _event: ControlEvent<'_>) {}
}

/// A no-op event sink used in tests that do not need backend events.
/// Production pane relays install a real sink (`PaneEventSink`) so OSC 0/2
/// titles and OSC 52 clipboard writes flow through to clients.
///
/// Gated behind `test-util` (and this crate's own `test`) so downstream test
/// builds — kmuxd's relay/app unit tests — can construct a `TermState` without
/// pulling it into production binaries.
#[cfg(any(test, feature = "test-util"))]
pub struct NullEventSink;
#[cfg(any(test, feature = "test-util"))]
impl BackendEventSink for NullEventSink {}

/// Configuration passed to [`TerminalBackend::new`].
#[allow(dead_code)]
pub struct BackendConfig {
    pub size: BackendSize,
    /// Live kitty-graphics/keyboard toggles. Populated on every
    /// attach/detach; libghostty-vt parses these sequences unconditionally,
    /// but see [`CapabilityHandles`] for the full rationale.
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

    /// Human-readable name of this backend (e.g. `"ghostty"`).
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
    /// [`ScrollbackLine`] of length `cols`, shared by reference so the mirror
    /// and the outgoing message need not deep-copy it (issue #182).
    fn read_history_lines(
        &self,
        _start: usize,
        _count: usize,
        _cols: usize,
    ) -> Vec<ScrollbackLine> {
        vec![]
    }

    /// Encode a structured key event into terminal escape bytes using the
    /// backend's live state (DECCKM, kitty kbd flags, modifyOtherKeys, …).
    ///
    /// Default returns an empty `Vec` — backends that do not implement key
    /// encoding (e.g. test stubs) silently drop key events.
    fn encode_key_event(&self, _event: &KeyEvent) -> Vec<u8> {
        Vec::new()
    }
}
