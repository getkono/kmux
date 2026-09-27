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

use harness::{
    Cleanup, Daemon, SIZE, Sandbox, Screen, attach, connect_client, daemon_token, pid_alive,
    poll_until, read_pid_file, recv_until, wait_for_daemon,
};
use kmux_protocol::messages::{ClientMessage, ServerMessage};

/// What the suite's session prints before it settles into `sleep`, so a test
/// can look for it on the screen afterwards.
const MARKER: &str = "PRINTED_BEFORE_THE_HANDOFF";

/// Connect over the data UDS and create a session whose pane records its
/// shell's PID to `pidfile` and prints [`MARKER`], then drop the client. The
/// session persists server-side. Returns the session's first pane and the PID.
async fn create_session_with_recorded_child(
    sandbox: &Sandbox,
    token: &str,
    pidfile: &Path,
) -> (String, i32) {
    let mut client = connect_client(sandbox, token).await;
    // `exec sleep` so the recorded PID *is* the long-lived process the handoff must
    // keep alive (no intermediate `sh` that could exit and change the PID).
    let script = format!(
        "echo $$ > {}; echo {MARKER}; exec sleep 600",
        pidfile.display()
    );
    let sent = client.tx.send(ClientMessage::SessionCreate {
        request_id: 1,
        name: Some("e2e".into()),
        peer: None,
        cwd: Some(sandbox.path().display().to_string()),
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), script],
        size: SIZE,
    });
    assert!(sent.is_ok(), "send SessionCreate");

    let created = recv_until(&mut client.rx, harness::E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await;
    let Some(ServerMessage::SessionCreated { entry, .. }) = created else {
        panic!("expected a SessionCreated ack, got {created:?}");
    };
    let pid = read_pid_file(pidfile, harness::E2E_TIMEOUT)
        .await
        .expect("shell wrote its PID");
    (format!("{}/0", entry.meta.word_id), pid)
}

/// B1: a real cross-process `restart` migrates the live shell — same process, new
/// daemon — exercising `spawn_successor` → `SCM_RIGHTS` → `restore_with_handoff`.
#[tokio::test]
async fn live_restart_preserves_running_shell_across_processes() {
    live_restart_preserves_the_shell(false).await.unwrap();
}

/// B1 with the pane in an isolated VT worker (issue #207): the worker parks
/// its PTY reader for the final checkpoint (`Hold`/`Held` over the worker
/// protocol) and the handoff still commits with the shell alive.
#[tokio::test]
async fn live_restart_preserves_a_worker_panes_shell() {
    live_restart_preserves_the_shell(true).await.unwrap();
}

/// B1's body, for a predecessor with isolated panes or not.
async fn live_restart_preserves_the_shell(isolated: bool) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();

    let predecessor = Daemon::new(&sandbox);
    let predecessor = if isolated {
        predecessor.isolated()
    } else {
        predecessor
    };
    let old_pid = predecessor.spawn(None).await;
    cleanup.track(old_pid as i32);

    let token = daemon_token(&sandbox).await;
    let pidfile = sandbox.path().join("child.pid");
    let (pane, child) = create_session_with_recorded_child(&sandbox, &token, &pidfile).await;
    cleanup.track(child);
    assert!(pid_alive(child), "shell should be alive before the restart");
    // The marker is on screen before the restart, so finding it after
    // means the screen crossed over, not that it was printed late.
    let mut before = connect_client(&sandbox, &token).await;
    attach(&before, &pane, None);
    assert!(Screen::new(&pane).follow_until(&mut before, MARKER).await);
    drop(before);

    let accepted = kmux_client::daemon::restart_daemon_at(&sandbox.socket_path())
        .await
        .context("restart control request")?;
    assert!(
        matches!(accepted, kmux_client::daemon::RestartReply::Accepted { .. }),
        "daemon should accept the graceful handoff"
    );

    let new_pid = wait_for_daemon(&sandbox, Some(old_pid))
        .await
        .context("a successor daemon should take over")?;
    cleanup.track(new_pid as i32);
    assert_ne!(new_pid, old_pid, "the successor must have a distinct PID");
    assert!(
        poll_until(harness::E2E_TIMEOUT, || !pid_alive(old_pid as i32)).await,
        "the old daemon should exit after releasing its sockets"
    );

    // Headline invariant: the SAME shell process survived the cross-process upgrade.
    assert!(
        pid_alive(child),
        "the running shell must survive the live restart"
    );
    // And so does what it put on screen: a client of the successor sees the
    // marker the shell printed only before the handoff.
    let mut after = connect_client(&sandbox, &token).await;
    attach(&after, &pane, None);
    let mut screen = Screen::new(&pane);
    assert!(
        screen.follow_until(&mut after, MARKER).await,
        "the screen must survive the live restart, but shows {:?}",
        screen.text().trim_end()
    );

    let _ = kmux_client::daemon::stop_daemon_at(&sandbox.socket_path()).await;
    Ok(())
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
    let (_, child) = create_session_with_recorded_child(&sandbox, &token, &pidfile).await;
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
    assert!(
        matches!(accepted, kmux_client::daemon::RestartReply::Accepted { .. }),
        "daemon should accept the graceful handoff"
    );

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

/// B3 (issue #207): a successor that finds no handoff socket while another
/// daemon still serves stands down — exiting with its own code, without
/// restoring or binding anything — instead of serving beside it.
#[tokio::test]
async fn a_successor_without_a_predecessor_socket_stands_down_while_one_serves() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    let serving = Daemon::new(&sandbox).spawn(None).await;
    cleanup.track(serving.cast_signed());

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_kmuxd"));
    cmd.args([
        "--daemon",
        "--handoff",
        "--bind",
        "127.0.0.1",
        "--port",
        "0",
    ])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    sandbox.env(&mut cmd);
    let mut successor = cmd.spawn().expect("spawn a successor");

    let exited = poll_until(harness::E2E_TIMEOUT, || {
        matches!(successor.try_wait(), Ok(Some(_)))
    })
    .await;
    if !exited {
        let _ = successor.kill();
    }
    let status = successor.wait().expect("reaped");
    assert!(exited, "the successor must stand down, not serve");
    assert_eq!(
        status.code(),
        Some(kmux_protocol::control_rpc::HANDOFF_STOOD_DOWN_EXIT_CODE)
    );
    let status = kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
        .await
        .expect("the serving daemon still answers");
    assert_eq!(status.pid, serving, "and it is the one that was serving");

    let _ = kmux_client::daemon::stop_daemon_at(&sandbox.socket_path()).await;
}
