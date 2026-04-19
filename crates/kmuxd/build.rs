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
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");

    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
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
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_DATE={date}");

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_PROFILE={profile}");

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
