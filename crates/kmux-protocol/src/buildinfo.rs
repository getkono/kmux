//! Build identity of the binary linking this crate, captured at compile time by
//! this crate's `build.rs`.
//!
//! Because the whole workspace builds from one git tree, kmux-protocol's values
//! represent whatever binary links it (the `kmux` CLI, `kmux-gtk`, `kmux-swift`),
//! so every `ClientMessage::Auth` construction site can report a consistent
//! client build identity in one place — used to detect client↔daemon build skew
//! that a matching `PROTOCOL_VERSION` alone cannot (two builds of the same
//! protocol but different commits).

/// Short git commit the client binary was built from (or `"unknown"`).
pub fn git_sha() -> &'static str {
    env!("BUILD_GIT_SHA")
}

/// Whether the client build had uncommitted changes at build time.
pub fn git_dirty() -> bool {
    !env!("BUILD_GIT_DIRTY_SUFFIX").is_empty()
}

/// Cargo profile of the client build (`"debug"` / `"release"`).
pub fn build_profile() -> &'static str {
    env!("BUILD_PROFILE")
}

/// The build fingerprint `<sha>` or `<sha>-dirty` — what `kmux clients` shows and
/// `kmux client status` compares against the CLI's own commit.
pub fn fingerprint() -> String {
    if git_dirty() {
        format!("{}-dirty", git_sha())
    } else {
        git_sha().to_string()
    }
}
