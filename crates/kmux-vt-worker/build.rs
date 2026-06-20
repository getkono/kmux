//! Embeds `libkmux_ghostty`'s install dir as an rpath in the kmux-vt-worker
//! binary, exactly like `kmuxd/build.rs`.
//!
//! `cargo:rustc-link-arg` emitted from a dependency's build script applies only
//! to that dependency's own targets, not to downstream binaries — so the rpath
//! emitted by `kmux-ghostty-sys/build.rs` never lands in this binary. We read
//! the lib dir exported via `DEP_KMUX_GHOSTTY_LIB_DIR` (from the `links` key on
//! kmux-ghostty-sys, a direct dependency) and emit the rpath from here instead.

fn main() {
    println!("cargo:rerun-if-env-changed=DEP_KMUX_GHOSTTY_LIB_DIR");
    if let Ok(lib_dir) = std::env::var("DEP_KMUX_GHOSTTY_LIB_DIR") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{lib_dir}");
    }
}
