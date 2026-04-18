use crate::backend::{BackendConfig, TerminalBackend as _};
pub use crate::diff_engine::DiffEngine;

#[cfg(feature = "backend-ghostty")]
pub use crate::backend::ghostty::GhosttyBackend;
#[cfg(all(feature = "backend-wezterm", not(feature = "backend-ghostty")))]
pub use crate::backend::wezterm::WezTermBackend;

// Backend selection priority: `backend-ghostty` (the forward path) wins when
// both features are compiled in, so a developer running
// `--features backend-ghostty` gets ghostty without having to also pass
// `--no-default-features`. Once commit 5 flips the default and commit 6
// deletes wezterm, this conditional collapses to a single `pub use`.
#[cfg(feature = "backend-ghostty")]
pub type ActiveBackend = GhosttyBackend;
#[cfg(all(feature = "backend-wezterm", not(feature = "backend-ghostty")))]
pub type ActiveBackend = WezTermBackend;

pub type TermState = DiffEngine<ActiveBackend>;

/// Human-readable name of the active terminal backend.
pub fn backend_name() -> &'static str {
    ActiveBackend::name()
}

/// Create a new `TermState` from a [`BackendConfig`].
pub fn new_term_state(cfg: BackendConfig) -> TermState {
    DiffEngine::new(ActiveBackend::new(cfg))
}
