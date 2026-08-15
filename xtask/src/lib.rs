//! Workspace quality tooling.
//!
//! This crate exists because kmux's quality gates consume structured data —
//! clippy's `--message-format=json` stream, cargo-mutants' `outcomes.json`, and
//! `cargo metadata`'s resolve graph. Doing that in shell would mean pinning a
//! JSON processor as a new tool dependency and writing brittle filters; doing it
//! here reuses the already-pinned toolchain, adds no third-party crates, and
//! makes the comparison logic itself unit-testable — which matters, because a
//! bug in a gate is silent by construction.
//!
//! It lives at `/xtask` rather than `crates/` on purpose: `crates/*` is the
//! product surface that [docs/crate-usage.md](../../docs/crate-usage.md) maps,
//! and the `crates/*` glob in the workspace manifest would otherwise sweep
//! tooling into every table and every gate.
//!
//! The architecture assertions are a test target rather than a command —
//! `xtask/tests/dependency_direction.rs` — so they ride the existing
//! `mise run test`, CI, and the pre-push hook instead of needing their own step.

pub mod graph;
