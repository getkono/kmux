//! Builds `libkmux_ghostty.a` — the Zig wrapper that exposes a kmux-owned
//! C ABI around libghostty-vt.
//!
//! Cargo's `build.rs` invokes this via `zig build` with `--prefix`, the zig
//! cache dirs, and `-Doptimize=...`. The installed artifact is linked by Rust
//! as `static=kmux_ghostty`.

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
    });
    wrapper_mod.addImport("ghostty_vt", ghostty_vt);

    const lib = b.addLibrary(.{
        .name = "kmux_ghostty",
        .root_module = wrapper_mod,
        .linkage = .static,
    });

    b.installArtifact(lib);
}
