//! Raw FFI bindings to `libkmux_ghostty` — a Zig static library that wraps
//! libghostty-vt and exports a kmux-owned, stable C ABI.
//!
//! This crate is intentionally thin: only `#[repr(C)]` types and `extern "C"`
//! declarations live here. The safe Rust surface is in `kmux-ghostty`.
//!
//! FFI invariants enforced by every binding in this module:
//! - No ownership transfer across the boundary in either direction.
//! - All pointer parameters are borrowed; valid only for the duration of the
//!   individual call (or, for event-sink callbacks, only for the callback).
//! - Output buffers are caller-allocated; the Zig side never allocates memory
//!   that Rust is expected to free.
//! - `kmux_ghostty_term` is opaque; construct it via `kmux_ghostty_new` and
//!   destroy it with `kmux_ghostty_free`.

#![deny(missing_debug_implementations)]

/// ABI version expected by this Rust crate. The Zig wrapper exports the same
/// constant via [`kmux_ghostty_abi_version`]. Mismatch is a build-time
/// inconsistency — safe wrappers must panic on mismatch.
pub const EXPECTED_ABI_VERSION: u32 = 1;

unsafe extern "C" {
    /// Return the ABI version baked into `libkmux_ghostty.a` at build time.
    ///
    /// Callers should compare the result against [`EXPECTED_ABI_VERSION`]
    /// once on startup (or in the safe façade's constructor) and refuse to
    /// proceed on mismatch.
    pub fn kmux_ghostty_abi_version() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_version_matches_expected() {
        let v = unsafe { kmux_ghostty_abi_version() };
        assert_eq!(
            v, EXPECTED_ABI_VERSION,
            "libkmux_ghostty ABI ({v}) does not match EXPECTED_ABI_VERSION ({EXPECTED_ABI_VERSION})",
        );
    }
}
