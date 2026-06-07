//! GTK4 frontend for kmux.
//!
//! The GTK4 + libadwaita stack runs on Linux + macOS only, so the entire
//! implementation lives in the platform-gated [`imp`] module — gated once, at
//! its `mod` declaration below, rather than per item. On other targets this
//! binary is just the stub `main` that prints an explanation and exits. See the
//! [`imp`] module docs for the platform-gating rationale and the frontend
//! architecture (the toolkit-agnostic `FrontendDriver` run loop + GTK leaves).

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() {
    eprintln!(
        "kmux-gtk: the GTK GUI is supported only on Linux and macOS \
         (macOS needs Homebrew GTK4 + libadwaita)."
    );
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    imp::run()
}
