//! Frontend-agnostic interaction layer for the kmux client.
//!
//! `kmux-app` sits between [`kmux_client`] (the mechanism: session management,
//! transports, bootstrap, the terminal grid model) and the concrete frontends
//! (`kmux-tui`, `kmux-gtk`, …). It owns the *interaction policy* that is
//! independent of any UI toolkit:
//!
//! - the modal keymap (`Mode`) and the [`Action`] vocabulary keys resolve to,
//! - the `/`-command palette,
//! - the `AppCore` view-model and connection-orchestration state machine,
//! - the theme *spec* (a toolkit-neutral RGB palette) and config loading,
//! - the non-interactive CLI subcommands.
//!
//! Hard rule: nothing in this crate may depend on a UI toolkit (no `ratatui`,
//! `crossterm`, or `gtk`). Frontends convert this crate's toolkit-neutral types
//! (e.g. the RGB palette) to their own at the render leaf, and *drive* the
//! `AppCore` — `AppCore` is a passive state machine, it never owns the run loop.
//!
//! Modules are introduced incrementally as logic is extracted from the TUI
//! binary (see the migration plan).

/// Modal keymap (`Mode`), the [`mode::Action`] vocabulary, and key → action
/// resolution. Toolkit-agnostic: depends only on `kmux_client::key`.
pub mod mode;

/// Toolkit-neutral color palette ([`theme::Rgb`], [`theme::Theme`]) and the
/// built-in theme TOML parsing.
pub mod theme;

/// Client config file + theme resolution (returns the toolkit-neutral theme).
pub mod config;

/// Persisted recent-servers cache (frontend-agnostic).
pub mod recent_servers;

/// The `/`-command palette: registry, parsing, completion hints, execution.
pub mod cmd;

/// The frontend-agnostic client view-model ([`core::AppCore`]) and the
/// connection/session orchestration that drives it.
pub mod core;
