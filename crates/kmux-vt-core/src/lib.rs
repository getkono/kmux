//! Shared server-side VT pipeline.
//!
//! This crate holds the pure(-ish) terminal processing that both the daemon's
//! in-process path and the isolated `kmux-vt-worker` subprocess run, so the two
//! paths produce byte-identical diffs by construction (issue #126):
//!
//! - [`backend`] — the [`TerminalBackend`](backend::TerminalBackend) trait and
//!   its libghostty-vt implementation ([`GhosttyBackend`](backend::ghostty::GhosttyBackend)),
//!   the *only* FFI/`unsafe` surface in the VT path.
//! - [`diff_engine`] — [`DiffEngine`](diff_engine::DiffEngine), the frame-to-frame
//!   cell-diffing wrapper, plus the [`ScrollbackMirror`](diff_engine::ScrollbackMirror).
//! - [`term_state`] — the concrete `TermState = DiffEngine<GhosttyBackend>` alias.
//!
//! Everything here is safe Rust except inside `backend::ghostty`, where the
//! libghostty-vt FFI is reached. Isolating *this crate's `feed()` path* into a
//! separate process is what lets a libghostty-vt fault take down only one
//! session instead of the whole daemon. See `docs/architecture-process-isolation.md`.

pub mod backend;
pub mod diff_engine;
pub mod term_state;
