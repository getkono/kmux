//! Captures the build's git SHA, dirty flag, and cargo profile as compile-time
//! env vars. Because the whole workspace builds from one git tree, the values
//! kmux-protocol records represent whatever binary links it — so every
//! `ClientMessage::Auth` construction site can report a consistent client build
//! identity (issue: client↔daemon build skew) without its own build script.

fn main() {
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

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_PROFILE={profile}");

    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
