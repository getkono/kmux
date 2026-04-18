//! kmux wrapper around libghostty-vt. Exposes a small, kmux-owned C ABI
//! so the Rust side can drive a libghostty-vt `Terminal` without depending
//! on ghostty's unstable internal C ABI.
//!
//! Scaffolding (Commit 2): only the ABI version probe is exported. The full
//! Terminal/Stream/Handler wiring lands in the next commit. The ghostty-vt
//! import below is retained so the build pulls in and compiles ghostty's
//! terminal module — verifying the dependency graph end-to-end.

const std = @import("std");
const vt = @import("ghostty_vt");

/// Incremented whenever the exported C ABI changes in a non-backwards-compatible
/// way. The Rust side verifies a matching value in its safe-façade constructor.
pub const ABI_VERSION: u32 = 1;

export fn kmux_ghostty_abi_version() callconv(.c) u32 {
    return ABI_VERSION;
}

comptime {
    // Force the ghostty-vt module to be compiled in so we catch any import
    // mismatches at build time, even before the real FFI lands.
    _ = vt.Terminal;
}
