//! Frontend-agnostic interaction layer for the kmux client.
//!
//! `kmux-app` sits between [`kmux_client`] (the mechanism: session management,
//! transports, bootstrap, the terminal grid model) and the concrete frontends
//! (`kmux-gtk`, `kmux-swift`, …). It owns the *interaction policy* that is
//! independent of any UI toolkit:
//!
//! - the modal keymap (`Mode`) and the [`crate::mode::Action`] vocabulary keys resolve to,
//! - the `/`-command palette,
//! - the `AppCore` view-model and connection-orchestration state machine,
//! - the theme *spec* (a toolkit-neutral RGB palette) and config loading,
//! - the non-interactive CLI subcommands.
//!
//! Hard rule: nothing in this crate may depend on a UI toolkit (no `gtk`, no
//! native macOS bindings). Frontends convert this crate's toolkit-neutral types
//! (e.g. the RGB palette) to their own at the render leaf, and *drive* the
//! `AppCore` — `AppCore` is a passive state machine, it never owns the run loop.

/// Modal keymap (`Mode`), the [`mode::Action`] vocabulary, and key → action
/// resolution. Toolkit-agnostic: depends only on `kmux_client::key`.
pub mod mode;

/// Toolkit-agnostic tiling-layout resolver: turns a server-authoritative
/// [`kmux_protocol::messages::LayoutNode`] tree + a window size into per-pane
/// cell rectangles, deterministically. Shared by every frontend so all clients
/// compute identical geometry (a hard requirement for PTY size negotiation).
pub mod layout;

/// Toolkit-neutral color palette ([`theme::Rgb`], [`theme::Theme`]) and the
/// built-in theme TOML parsing.
pub mod theme;

/// Toolkit-neutral terminal appearance/font settings ([`appearance::Appearance`])
/// that each frontend converts to its own font/metrics types at the render leaf.
pub mod appearance;

/// Client config file + theme resolution (returns the toolkit-neutral theme).
pub mod config;

/// Persisted recent-servers cache (frontend-agnostic).
pub mod recent_servers;

/// The `/`-command palette: registry, parsing, completion hints, execution.
pub mod cmd;

/// CLI argument definitions (clap).
pub mod cli;

/// Dynamic shell-completion value sources (clap_complete `unstable-dynamic`).
pub mod completion;

/// Terminal capability detection (env-based; frontend-agnostic).
pub mod host_caps;

/// Render diagnostic suite (`kmux diagnostic <test>`): named test patterns and
/// the in-session emitter (issue #145).
pub mod diagnostic;

/// Non-interactive subcommands (`ls`, `daemon`, `--dry-run`) and target parsing.
pub mod subcommands;

/// Shared CLI front door (`run_cli`) used by every frontend binary.
pub mod launch;

/// Single source of build + version metadata ([`version::VersionInfo`]) surfaced
/// by `kmux -V` and both GUIs' "About" panels.
/// Rendering numbers the way a person reads them, in the two styles kmux
/// shows: full units for CLI output, compact for a fixed-width GUI column.
pub mod humanize;

pub mod version;

/// The frontend-agnostic client view-model ([`core::AppCore`]) and the
/// connection/session orchestration that drives it.
pub mod core;

/// The toolkit-agnostic run-loop driver ([`driver::FrontendDriver`]) that owns
/// the network channels + pump shared by every frontend.
pub mod driver;

/// Serializes process-environment mutation (`XDG_CONFIG_HOME` / `XDG_RUNTIME_DIR`)
/// across the whole `kmux-app` test binary, since several modules' tests redirect
/// the same vars and Cargo runs tests in parallel threads within one process.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
