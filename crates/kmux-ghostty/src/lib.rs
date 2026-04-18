//! Safe Rust façade over [`kmux-ghostty-sys`]. Wraps the kmux-owned C ABI
//! exposed by `libkmux_ghostty` (a Zig wrapper around libghostty-vt v1.3.1)
//! in idiomatic, lifetime-checked types.
//!
//! Scaffolding (Commit 2): only the ABI version probe is exposed. Real
//! `Terminal` construction, `feed`, `fill_cells`, event bridging and the
//! `Send` assertion land in the next commit.

#![deny(missing_debug_implementations)]

/// ABI version expected by this build of the safe façade. Verified against
/// the Zig side at runtime by [`check_abi_version`].
pub const EXPECTED_ABI_VERSION: u32 = kmux_ghostty_sys::EXPECTED_ABI_VERSION;

/// Return the ABI version reported by `libkmux_ghostty`.
#[must_use]
pub fn abi_version() -> u32 {
    unsafe { kmux_ghostty_sys::kmux_ghostty_abi_version() }
}

/// Panic if the Zig-side ABI version does not match [`EXPECTED_ABI_VERSION`].
///
/// This is a build-graph consistency check: a mismatch means the Rust crate
/// and the static library were built against different ABI revisions, which
/// is never recoverable at runtime.
pub fn check_abi_version() {
    let got = abi_version();
    assert_eq!(
        got, EXPECTED_ABI_VERSION,
        "libkmux_ghostty ABI mismatch: linked version is {got}, \
         but this crate expects {EXPECTED_ABI_VERSION}. \
         Rebuild with `cargo clean -p kmux-ghostty-sys`.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_probe_matches() {
        assert_eq!(abi_version(), EXPECTED_ABI_VERSION);
        check_abi_version();
    }
}
