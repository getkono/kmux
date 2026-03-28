pub use crate::diff_engine::DiffEngine;

#[cfg(feature = "backend-alacritty")]
pub use crate::backend::alacritty::AlacrittyBackend;

#[cfg(feature = "backend-termwiz")]
pub use crate::backend::termwiz::TermwizBackend;

// Mutual exclusivity of backend features is enforced in build.rs.

#[cfg(feature = "backend-alacritty")]
pub type TermState = DiffEngine<AlacrittyBackend>;

#[cfg(feature = "backend-termwiz")]
pub type TermState = DiffEngine<TermwizBackend>;

/// Name of the active terminal backend (for logging).
#[cfg(feature = "backend-alacritty")]
pub const BACKEND_NAME: &str = "alacritty";

#[cfg(feature = "backend-termwiz")]
pub const BACKEND_NAME: &str = "termwiz";

/// Create a new `TermState` with the active backend for the given dimensions.
#[cfg(feature = "backend-alacritty")]
pub fn new_term_state(rows: u16, cols: u16) -> TermState {
    DiffEngine::new(AlacrittyBackend::new(rows, cols))
}

#[cfg(feature = "backend-termwiz")]
pub fn new_term_state(rows: u16, cols: u16) -> TermState {
    DiffEngine::new(TermwizBackend::new(rows, cols))
}
