//! Command palette: an extensible registry-driven `/`-prefixed mini-language
//! for the TUI. See `docs/command-mode.md` for the design.
//!
//! Module layout:
//! - [`spec`]: data types for command definitions and errors.
//! - [`parse`]: string → resolved command + args.
//! - [`hint`]: live autocomplete generation.
//! - [`registry`]: the static table of built-in commands.
//! - [`exec`]: dispatch a parsed command into `App`.

pub mod exec;
pub mod hint;
pub mod parse;
pub mod registry;
pub mod spec;
