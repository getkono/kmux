use crate::backend::{BackendConfig, TerminalBackend as _};
pub use crate::diff_engine::DiffEngine;

pub use crate::backend::ghostty::GhosttyBackend;

pub type ActiveBackend = GhosttyBackend;

pub type TermState = DiffEngine<ActiveBackend>;

/// Human-readable name of the active terminal backend.
pub fn backend_name() -> &'static str {
    ActiveBackend::name()
}

/// Create a new `TermState` from a [`BackendConfig`].
pub fn new_term_state(cfg: BackendConfig) -> TermState {
    DiffEngine::new(ActiveBackend::new(cfg))
}
