pub use crate::backend::wezterm::WezTermBackend;
pub use crate::diff_engine::DiffEngine;

pub type TermState = DiffEngine<WezTermBackend>;

/// Name of the active terminal backend (for logging).
pub const BACKEND_NAME: &str = "wezterm";

/// Create a new `TermState` for the given dimensions.
pub fn new_term_state(rows: u16, cols: u16) -> TermState {
    DiffEngine::new(WezTermBackend::new(rows, cols))
}
