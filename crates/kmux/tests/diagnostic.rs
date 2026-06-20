//! Tests for the `kmux diagnostic` subcommand (issue #145) exercised through the
//! real `kmux` binary — the same path the launched session and a user run.
//!
//! The interactive `kmux diagnostic <test>` form hands off to the GUI and is
//! covered by the `kmux-app` unit tests + manual verification; here we cover the
//! two non-interactive forms that short-circuit before any GUI handoff: the
//! catalogue listing and the `--emit` pattern writer.

use std::process::{Command, Stdio};

/// `kmux diagnostic --emit glyphs` writes the pattern to stdout and exits. With
/// stdin closed (EOF) the "press Enter to exit" hold returns immediately, so the
/// test is deterministic.
#[test]
fn emit_writes_pattern_and_exits() {
    let out = Command::new(env!("CARGO_BIN_EXE_kmux"))
        .args(["diagnostic", "glyphs", "--emit"])
        .stdin(Stdio::null())
        .output()
        .expect("run kmux diagnostic --emit");

    assert!(
        out.status.success(),
        "expected success, got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Signature content of the glyph pattern + the exit footer.
    assert!(
        stdout.contains("Printable ASCII"),
        "missing glyph pattern in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Diagnostic: glyphs"),
        "missing exit footer in stdout:\n{stdout}"
    );
}

/// `kmux diagnostic` with no test lists the catalogue and exits 0, without trying
/// to launch the desktop GUI.
#[test]
fn no_test_lists_catalogue() {
    let out = Command::new(env!("CARGO_BIN_EXE_kmux"))
        .arg("diagnostic")
        .stdin(Stdio::null())
        .output()
        .expect("run kmux diagnostic");

    assert!(out.status.success(), "non-zero exit {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for name in ["glyphs", "attrs", "colors", "unicode", "boxes", "all"] {
        assert!(
            stdout.contains(name),
            "catalogue missing `{name}`:\n{stdout}"
        );
    }
}

/// An unknown test name is rejected by clap (exit non-zero), not silently
/// launched.
#[test]
fn unknown_test_is_rejected() {
    let out = Command::new(env!("CARGO_BIN_EXE_kmux"))
        .args(["diagnostic", "not-a-real-test", "--emit"])
        .stdin(Stdio::null())
        .output()
        .expect("run kmux diagnostic");
    assert!(
        !out.status.success(),
        "expected a parse error for an unknown test name"
    );
}
