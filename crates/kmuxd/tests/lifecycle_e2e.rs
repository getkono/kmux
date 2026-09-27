//! A pane's and a daemon's lifecycle, end to end (issue #211): a shell that
//! exits, a client that comes back, and a daemon that stops.
//!
//! Each runs a real `kmuxd` because what it checks is only visible across
//! the process boundary: a real shell's exit status, the diffs a real VT
//! produced while nobody watched, and the files a real process leaves behind
//! when it exits. The pieces are unit-tested where they live —
//! `compute_replay` in `app/mod.rs`, `SocketGuard` in `daemon.rs` — and these
//! pin that they are wired together.

#![cfg(unix)]

mod harness;

use harness::{
    Cleanup, Daemon, E2E_TIMEOUT, Sandbox, connect_client, daemon_token, pid_alive, poll_until,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// A serving daemon has a control socket and a pid file in `sandbox`.
fn assert_serving(sandbox: &Sandbox) {
    assert!(sandbox.socket_path().exists(), "no control socket");
    assert!(sandbox.pid_path().exists(), "no pid file");
}

/// The daemon `pid` exits and leaves nothing behind for the next one to trip
/// over: no control socket to mistake for a live one, and no pid file naming
/// a process that is gone.
async fn assert_exits_leaving_nothing(sandbox: &Sandbox, pid: u32) {
    let exited = poll_until(E2E_TIMEOUT, || !pid_alive(pid.cast_signed())).await;
    assert!(exited, "the daemon exits:\n{}", sandbox.daemon_log());
    assert!(
        !sandbox.socket_path().exists(),
        "control socket left behind"
    );
    assert!(!sandbox.pid_path().exists(), "pid file left behind");
}

#[tokio::test]
async fn a_daemon_stopped_by_sigterm_leaves_no_socket_or_pid_file() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    let pid = Daemon::new(&sandbox).spawn(None).await;
    cleanup.track(pid.cast_signed());
    assert_serving(&sandbox);

    kill(Pid::from_raw(pid.cast_signed()), Signal::SIGTERM).expect("SIGTERM");
    assert_exits_leaving_nothing(&sandbox, pid).await;
}

/// Idle shutdown starts counting when the last client leaves, so one comes
/// and goes.
#[tokio::test]
async fn an_idle_daemon_shutting_down_leaves_no_socket_or_pid_file() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    let pid = Daemon::new(&sandbox)
        .config("[daemon]\nidle_shutdown_secs = 1\n")
        .spawn(None)
        .await;
    cleanup.track(pid.cast_signed());
    assert_serving(&sandbox);

    drop(connect_client(&sandbox, &daemon_token(&sandbox).await).await);
    assert_exits_leaving_nothing(&sandbox, pid).await;
}
