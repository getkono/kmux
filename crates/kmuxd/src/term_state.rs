use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub use crate::backend::wezterm::WezTermBackend;
pub use crate::diff_engine::DiffEngine;

pub type TermState = DiffEngine<WezTermBackend>;

/// Name of the active terminal backend (for logging).
pub const BACKEND_NAME: &str = "wezterm";

/// Create a new `TermState` for the given dimensions.
///
/// `kitty_graphics` and `kitty_keyboard` are live atomics shared with the
/// calling `PaneRelay`; the daemon updates them on client attach/detach.
pub fn new_term_state(
    rows: u16,
    cols: u16,
    kitty_graphics: Arc<AtomicBool>,
    kitty_keyboard: Arc<AtomicBool>,
) -> TermState {
    DiffEngine::new(WezTermBackend::new(
        rows,
        cols,
        kitty_graphics,
        kitty_keyboard,
    ))
}
