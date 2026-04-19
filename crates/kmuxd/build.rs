//! Embeds `libkmux_ghostty.so`'s install dir as an rpath in the kmuxd binary.
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
}
