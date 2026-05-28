//! Builds `libkmux_ghostty` — the Zig wrapper that exposes a kmux-owned
//! C ABI around libghostty-vt. It is emitted as a shared library (see the
//! `.linkage = .dynamic` note below).
//!
//! Cargo's `build.rs` invokes this via `zig build` with `--prefix`, the zig
//! cache dirs, and `-Doptimize=...`. The installed artifact is linked by Rust
//! as `dylib=kmux_ghostty`.

const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    // Pull in ghostty's Zig module graph. The `.path` dep in build.zig.zon
    // points at `vendor/ghostty` (the pinned v1.3.1 submodule).
    const ghostty_dep = b.dependency("ghostty", .{
        .target = target,
        .optimize = optimize,
    });
    const ghostty_vt = ghostty_dep.module("ghostty-vt");

    const wrapper_mod = b.createModule(.{
        .root_source_file = b.path("src/wrapper.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
        // Ghostty's SIMD helpers (src/simd/*.cpp) pull libc++ into the link.
        // Marking the wrapper module as a libc++ consumer ensures Zig resolves
        // libc++/libc++abi itself when producing the final shared library — no
        // runtime dependency on a system libc++ is required.
        .link_libcpp = true,
    });
    wrapper_mod.addImport("ghostty_vt", ghostty_vt);

    // Ship as a shared library: the archive produced for a `.static` link
    // would not bundle libc++/libc++abi, and rust-lld would fail to resolve
    // the C++ stdlib symbols at final-link time. A `.so` has the C++ runtime
    // baked in, mirroring how ghostty distributes `libghostty-vt.so`.
    const lib = b.addLibrary(.{
        .name = "kmux_ghostty",
        .root_module = wrapper_mod,
        .linkage = .dynamic,
    });

    b.installArtifact(lib);

    const tests = b.addTest(.{ .root_module = wrapper_mod });
    const run_tests = b.addRunArtifact(tests);
    const test_step = b.step("test", "Run wrapper.zig unit tests");
    test_step.dependOn(&run_tests.step);
}
