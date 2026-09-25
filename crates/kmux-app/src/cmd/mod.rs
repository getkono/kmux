//! Command palette: an extensible registry-driven `/`-prefixed mini-language
//! for the client. See `docs/command-mode.md` for the design.
//!
//! Module layout:
//! - `spec`: data types for command definitions and errors.
//! - `parse`: string → resolved command + args.
//! - [`crate::cmd::hint`]: live autocomplete generation.
//! - `registry`: the static table of built-in commands.
//! - `exec`: dispatch a parsed command into `App`.

pub(crate) mod exec;
pub mod hint;
pub(crate) mod parse;
pub(crate) mod registry;
pub(crate) mod spec;
