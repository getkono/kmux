pub use crate::diff_engine::DiffEngine;

#[cfg(feature = "backend-alacritty")]
pub use crate::backend::alacritty::AlacrittyBackend;

/// Backward-compatible type alias: `TermState` is a `DiffEngine` wrapping
/// the default alacritty backend.
#[cfg(feature = "backend-alacritty")]
pub type TermState = DiffEngine<AlacrittyBackend>;
