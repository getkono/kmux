//! Embeds `libkmux_ghostty.so`'s install dir as an rpath in the kmuxd binary,
//! and captures build metadata (git SHA, dirty flag, date, profile) as env vars.
//!
//! `cargo:rustc-link-arg` emitted from a dependency's build script applies only
//! to that dependency's own targets, not to downstream binaries — so the rpath
//! emitted by `kmux-ghostty-sys/build.rs` never lands in `kmuxd`. We read the
//! lib dir exported via `DEP_KMUX_GHOSTTY_LIB_DIR` (from the `links` key on
//! kmux-ghostty-sys) and emit the rpath from this crate instead.

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_KMUX_GHOSTTY_LIB_DIR");
    if let Ok(lib_dir) = std::env::var("DEP_KMUX_GHOSTTY_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
    emit_build_info();
}

fn emit_build_info() {
    use std::process::Command;

    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| !o.stdout.is_empty());
    println!(
        "cargo:rustc-env=BUILD_GIT_DIRTY_SUFFIX={}",
        if dirty { "-dirty" } else { "" }
    );

    let date = Command::new("date")
        .arg("+%Y-%m-%d")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=BUILD_DATE={date}");

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_PROFILE={profile}");

    // Full rustc version string, e.g. "rustc 1.86.0 (...)". Use cargo's `RUSTC`
    // env (the toolchain actually compiling this crate) so it matches the build.
    let rustc = std::env::var("RUSTC")
        .ok()
        .and_then(|rustc| Command::new(rustc).arg("--version").output().ok())
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=BUILD_RUSTC_VERSION={rustc}");

    // Full UTC build timestamp (ISO-8601) — a superset of BUILD_DATE.
    let timestamp = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |s| s.trim().to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={timestamp}");

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
