//! Bake the cargo `target/<profile>` directory into the crate so a **debug**
//! build can locate its matching `target/debug/kmuxd` even when the running
//! executable lives outside the cargo target tree — e.g. the SwiftPM `.build`
//! app bundle, whose `current_exe()` has no `kmuxd` sibling.
//!
//! Without this a `cargo run` / `swift run` GUI falls through to the `$PATH`
//! walk and picks up an installed *release* `~/.cargo/bin/kmuxd`, which a debug
//! client can never talk to (the two profiles use separate runtime dirs). See
//! `find_server_binary` in `src/daemon/lifecycle.rs`.

use std::path::Path;

fn main() {
    // `OUT_DIR` is `<target>/<profile>/build/<pkg>-<hash>/out`; three parents up
    // is `<target>/<profile>` (where the workspace binaries land). Deriving it
    // from `OUT_DIR` rather than hard-coding "target/debug" keeps it correct
    // under `CARGO_TARGET_DIR` overrides and custom profile names.
    if let Ok(out_dir) = std::env::var("OUT_DIR")
        && let Some(profile_dir) = Path::new(&out_dir).ancestors().nth(3)
    {
        println!("cargo:rustc-env=KMUXD_TARGET_DIR={}", profile_dir.display());
    }
    println!("cargo:rerun-if-changed=build.rs");
}
