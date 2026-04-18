use crate::backend::{BackendConfig, TerminalBackend as _};
pub use crate::diff_engine::DiffEngine;

#[cfg(feature = "backend-wezterm")]
pub use crate::backend::wezterm::WezTermBackend;

#[cfg(feature = "backend-wezterm")]
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
