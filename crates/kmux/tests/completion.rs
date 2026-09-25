//! Smoke tests for the dynamic shell-completion hook wired into `run_cli`.
//!
//! These exercise the real `kmux` binary with the `COMPLETE` env var set, which
//! is how a shell drives `clap_complete`'s `CompleteEnv`. They prove the hook is
//! installed (and short-circuits before the GUI handoff) without depending on
//! the unstable internal `_CLAP_COMPLETE_*` request protocol — we assert on the
//! stable registration-script output. Value-level completion is covered by the
//! `kmux-app` unit tests in `completion.rs`.

use std::process::Command;

/// `COMPLETE=bash kmux` prints the bash registration script and exits 0, without
/// trying to launch the desktop GUI.
#[test]
fn complete_bash_emits_registration_script() {
    let out = Command::new(env!("CARGO_BIN_EXE_kmux"))
        .env("COMPLETE", "bash")
        .output()
        .expect("run kmux");

    assert!(
        out.status.success(),
        "expected success, got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("_clap_complete_kmux") && stdout.contains("COMPLETE=\"bash\""),
        "missing bash registration script in stdout:\n{stdout}"
    );
}

/// The same mechanism works for zsh and fish (each emits a shell-specific script).
#[test]
fn complete_zsh_and_fish_emit_scripts() {
    for shell in ["zsh", "fish"] {
        let out = Command::new(env!("CARGO_BIN_EXE_kmux"))
            .env("COMPLETE", shell)
            .output()
            .expect("run kmux");
        assert!(
            out.status.success(),
            "{shell}: non-zero exit {:?}",
            out.status
        );
        assert!(
            !out.stdout.is_empty(),
            "{shell}: expected a non-empty completion script"
        );
    }
}
