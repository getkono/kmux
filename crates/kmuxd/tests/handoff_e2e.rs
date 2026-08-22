//! Cross-process integration tests for the graceful daemon handoff that backs the
//! live daemon upgrade (issues #35 / #36).
//!
//! These spawn a *real* `kmuxd` (via `CARGO_BIN_EXE_kmuxd`), open the data socket
//! to create a session with a long-lived shell, trigger `restart` over the control
//! socket, and assert that a successor process takes over with the running shell
//! intact. Unlike the in-process `live_pty_migrates_with_same_pid` unit test (which
//! hand-transfers an fd between two `ServerApp`s in one process), they exercise the
//! actual fork / exec / daemonize / `SCM_RIGHTS` path — including the in-place
//! binary swap that `mise run upgrade-daemon` performs.

#![cfg(unix)]

mod harness;

use std::path::Path;
use std::time::Duration;

use harness::{
    Cleanup, Daemon, SIZE, Sandbox, connect_client, daemon_token, pid_alive, poll_until,
    read_pid_file, recv_until, wait_for_daemon,
};
use kmux_protocol::messages::{ClientMessage, ServerMessage};
use tokio::sync::mpsc;

/// Connect over the data UDS, create a session whose initial pane runs a shell that
/// records its own PID to `pidfile`, then drop the client. The session persists
/// server-side. Returns the shell's PID.
async fn create_session_with_recorded_child(
    sandbox: &Sandbox,
    token: &str,
    cwd: &Path,
    pidfile: &Path,
) -> i32 {
    let mut client = connect_client(sandbox, token).await;
    let mut srv_rx = std::mem::replace(&mut client.rx, mpsc::unbounded_channel().1);
    let client_tx = client.tx.clone();

    // `exec sleep` so the recorded PID *is* the long-lived process the handoff must
    // keep alive (no intermediate `sh` that could exit and change the PID).
    let script = format!("echo $$ > {}; exec sleep 600", pidfile.display());
    client_tx
        .send(ClientMessage::SessionCreate {
            request_id: 1,
            name: Some("e2e".into()),
            peer: None,
            cwd: Some(cwd.display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script],
            size: SIZE,
        })
        .expect("send SessionCreate");

    let created = recv_until(&mut srv_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await;
    assert!(created.is_some(), "expected a SessionCreated ack");

    let pid = read_pid_file(pidfile, Duration::from_secs(5)).expect("shell wrote its PID");
    // Drop the client connection — the server keeps the session alive without it.
    drop(client_tx);
    pid
}

/// B1: a real cross-process `restart` migrates the live shell — same process, new
/// daemon — exercising `spawn_successor` → `SCM_RIGHTS` → `restore_with_handoff`.
#[tokio::test]
async fn live_restart_preserves_running_shell_across_processes() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();

    let old_pid = Daemon::new(&sandbox).spawn(None).await;
    cleanup.track(old_pid as i32);

    let token = daemon_token(&sandbox).await;
    let pidfile = sandbox.path().join("child.pid");
    let child =
        create_session_with_recorded_child(&sandbox, &token, sandbox.path(), &pidfile).await;
    cleanup.track(child);
    assert!(pid_alive(child), "shell should be alive before the restart");
    assert!(
        kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
            .await
            .unwrap()
            .session_count
            >= 1,
        "the session should be present before the restart"
    );

    let accepted = kmux_client::daemon::restart_daemon_at(&sandbox.socket_path())
        .await
        .expect("restart control request");
    assert!(accepted, "daemon should accept the graceful handoff");

    let new_pid = wait_for_daemon(&sandbox, Some(old_pid))
        .await
        .expect("a successor daemon should take over");
    cleanup.track(new_pid as i32);
    assert_ne!(new_pid, old_pid, "the successor must have a distinct PID");
    assert!(
        poll_until(Duration::from_secs(15), || !pid_alive(old_pid as i32)).await,
        "the old daemon should exit after releasing its sockets"
    );

    // Headline invariant: the SAME shell process survived the cross-process upgrade.
    assert!(
        pid_alive(child),
        "the running shell must survive the live restart"
    );
    assert!(
        kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
            .await
            .unwrap()
            .session_count
            >= 1,
        "the session must persist across the restart"
    );

    let _ = kmux_client::daemon::stop_daemon_at(&sandbox.socket_path()).await;
}

/// B2: replacing the daemon binary in place (as `cargo install` does) before
/// `restart` still hands off. Regression guard for `resolve_successor_exe`: on Linux
/// the atomic rename unlinks the running inode, so `current_exe()` reads back as
/// `"<path> (deleted)"` — re-execing that literal path would ENOENT and silently
/// keep the old code running. Passes trivially on macOS (no marker); the assertion
/// has teeth on Linux.
#[tokio::test]
async fn in_place_binary_swap_still_hands_off() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();

    // Run from a writable copy so we can replace it in place mid-flight.
    let exe = sandbox.path().join("kmuxd");
    std::fs::copy(env!("CARGO_BIN_EXE_kmuxd"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    let old_pid = Daemon::new(&sandbox).exe(exe.clone()).spawn(None).await;
    cleanup.track(old_pid as i32);
    let token = daemon_token(&sandbox).await;
    let pidfile = sandbox.path().join("child.pid");
    let child =
        create_session_with_recorded_child(&sandbox, &token, sandbox.path(), &pidfile).await;
    cleanup.track(child);

    // Simulate `cargo install`'s atomic replace: stage a fresh copy and rename it
    // over the running binary (unlinking the running inode on Linux).
    let staged = sandbox.path().join("kmuxd.new");
    std::fs::copy(env!("CARGO_BIN_EXE_kmuxd"), &staged).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, &exe).unwrap();

    let accepted = kmux_client::daemon::restart_daemon_at(&sandbox.socket_path())
        .await
        .expect("restart control request");
    assert!(accepted, "daemon should accept the graceful handoff");

    let new_pid = wait_for_daemon(&sandbox, Some(old_pid))
        .await
        .expect("a successor must take over even after an in-place binary swap");
    cleanup.track(new_pid as i32);
    assert_ne!(new_pid, old_pid, "the successor must have a distinct PID");
    assert!(
        pid_alive(child),
        "the running shell must survive an in-place daemon upgrade"
    );

    let _ = kmux_client::daemon::stop_daemon_at(&sandbox.socket_path()).await;
}
