pub use crate::diff_engine::DiffEngine;

#[cfg(feature = "backend-alacritty")]
pub use crate::backend::alacritty::AlacrittyBackend;

#[cfg(feature = "backend-termwiz")]
pub use crate::backend::termwiz::TermwizBackend;

/// `TermState` type alias: picks the backend based on enabled features.
///
/// Priority: alacritty > termwiz. To use termwiz, compile with:
///   `cargo run -p smux-server --no-default-features --features backend-termwiz`
#[cfg(feature = "backend-alacritty")]
pub type TermState = DiffEngine<AlacrittyBackend>;

#[cfg(all(feature = "backend-termwiz", not(feature = "backend-alacritty")))]
pub type TermState = DiffEngine<TermwizBackend>;

/// Create a new `TermState` with the active backend for the given dimensions.
#[cfg(feature = "backend-alacritty")]
pub fn new_term_state(rows: u16, cols: u16) -> TermState {
    DiffEngine::new(AlacrittyBackend::new(rows, cols))
}

#[cfg(all(feature = "backend-termwiz", not(feature = "backend-alacritty")))]
pub fn new_term_state(rows: u16, cols: u16) -> TermState {
    DiffEngine::new(TermwizBackend::new(rows, cols))
}
