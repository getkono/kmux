//! Drives the Zig build that produces `libkmux_ghostty.a` (a static archive
//! wrapping libghostty-vt v1.3.1) and instructs Cargo to link it.
//!
//! Build-time invariants:
//! - The `vendor/ghostty` git submodule must be initialised before this runs.
//! - The host must provide `zig` 0.15.2 (pinned via `mise.toml`).
//! - All Zig cache state is confined to `$OUT_DIR`; we never pollute `$HOME`.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXPECTED_ZIG_VERSION: &str = "0.15.2";

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("cargo always sets CARGO_MANIFEST_DIR"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo always sets OUT_DIR"));

    let zig_src_dir = manifest_dir.join("zig");
    let ghostty_dir = manifest_dir
        .join("..")
        .join("..")
        .join("vendor")
        .join("ghostty");

    require_submodule(&ghostty_dir);
    emit_rerun_hints(&zig_src_dir, &ghostty_dir);

    let zig = resolve_zig();
    verify_zig_version(&zig);

    let optimize = match env::var("PROFILE").as_deref() {
        Ok("release") => "ReleaseFast",
        _ => "Debug",
    };

    let install_prefix = out_dir.join("install");
    let cache_dir = out_dir.join("zig-cache");
    let global_cache_dir = out_dir.join("zig-global");

    let status = Command::new(&zig)
        .current_dir(&zig_src_dir)
        .arg("build")
        .arg(format!("-Doptimize={optimize}"))
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(&cache_dir)
        .arg("--global-cache-dir")
        .arg(&global_cache_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke `zig build`: {e}"));

    if !status.success() {
        panic!("`zig build` for libkmux_ghostty failed (exit: {status:?})");
    }

    println!(
        "cargo:rustc-link-search=native={}",
        install_prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=kmux_ghostty");
}

fn require_submodule(ghostty_dir: &Path) {
    let marker = ghostty_dir.join("build.zig.zon");
    if !marker.exists() {
        panic!(
            "vendor/ghostty is not initialised (missing {}).\n\
             Run: `git submodule update --init` at the repo root.",
            marker.display()
        );
    }
}

fn emit_rerun_hints(zig_src_dir: &Path, ghostty_dir: &Path) {
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", zig_src_dir.display());
    println!(
        "cargo:rerun-if-changed={}",
        ghostty_dir.join("build.zig").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        ghostty_dir.join("build.zig.zon").display()
    );
}

fn resolve_zig() -> String {
    if let Some(z) = env::var_os("ZIG") {
        return z.to_string_lossy().into_owned();
    }
    "zig".to_string()
}

fn verify_zig_version(zig: &str) {
    let out = Command::new(zig)
        .arg("version")
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke `{zig} version`: {e}.\n\
             Install zig {EXPECTED_ZIG_VERSION} via `mise install` (see mise.toml).",
            )
        });
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if version != EXPECTED_ZIG_VERSION {
        panic!(
            "zig version mismatch: expected {EXPECTED_ZIG_VERSION}, found {version}.\n\
             Run `mise install` to align with the pin in mise.toml.",
        );
    }
}
