//! A pane's and a daemon's lifecycle, end to end (issue #211): a shell that
//! exits, a client that comes back, and a daemon that stops.
//!
//! Each runs a real `kmuxd` because what it checks is only visible across
//! the process boundary: a real shell's exit status, the diffs a real VT
//! produced while nobody watched, and the files a real process leaves behind
//! when it exits. The pieces are unit-tested where they live —
//! `compute_replay` in `app/attach.rs`, `SocketGuard` in `daemon.rs` — and these
//! pin that they are wired together.

#![cfg(unix)]

mod harness;

use harness::{
    Cleanup, Daemon, E2E_TIMEOUT, Sandbox, Screen, attach, connect_client, create_and_attach,
    daemon_token, pid_alive, poll_until, recv_until, type_into,
};
use kmux_protocol::messages::{ServerMessage, SessionEventMsg};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// A shell that exits tells the client watching its pane how: the status is
/// the shell's own, from the kernel, carried to the client as `PaneExited`.
#[tokio::test]
async fn a_shell_exit_reaches_the_attached_client_with_its_status() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    cleanup.track(Daemon::new(&sandbox).spawn(None).await.cast_signed());
    let mut client = connect_client(&sandbox, &daemon_token(&sandbox).await).await;
    // The shell waits for a line, so it exits only once the client is
    // attached to see it.
    let pane = create_and_attach(&mut client, 1, Some(&["/bin/sh", "-c", "read _; exit 7"])).await;
    let attached = recv_until(
        &mut client.rx,
        E2E_TIMEOUT,
        |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == pane),
    )
    .await;
    assert!(attached.is_some(), "attached to {pane}");

    type_into(&client, &pane, "\n");
    let exited = recv_until(&mut client.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::Event { event: SessionEventMsg::PaneExited { pane_id, .. } }
            if *pane_id == pane)
    })
    .await;

    let Some(ServerMessage::Event {
        event: SessionEventMsg::PaneExited { code, signal, .. },
    }) = exited
    else {
        panic!("no PaneExited for {pane}: {exited:?}");
    };
    assert_eq!((code, signal), (Some(7), None));
}

/// A client that comes back names the last seqno it applied, and is sent
/// exactly the diffs it missed — its screen catches up with no snapshot. One
/// that missed more than is worth replaying is reset with a fresh snapshot
/// instead. (The other way to fall that far behind, a seqno older than the
/// 10 MiB of diffs a pane keeps, is answered the same way; `compute_replay`'s
/// unit tests pin both.) The output the client misses is typed by a second
/// client that never leaves, and that client's screen says when it has
/// happened.
#[tokio::test]
async fn a_returning_client_is_sent_what_it_missed_or_reset_when_too_far_behind() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    cleanup.track(Daemon::new(&sandbox).spawn(None).await.cast_signed());
    let token = daemon_token(&sandbox).await;
    let mut stayer = connect_client(&sandbox, &token).await;
    let pane = create_and_attach(&mut stayer, 1, Some(&["/bin/sh"])).await;
    let mut stayer_screen = Screen::new(&pane);
    // `$((6*7))` so the marker is on screen only once the shell has run the
    // line, not merely echoed what was typed.
    let mut run = async |line: &str, marker: &str| {
        type_into(&stayer, &pane, &format!("{line}\n"));
        assert!(
            stayer_screen.follow_until(&mut stayer, marker).await,
            "{marker}"
        );
    };

    let mut leaver = connect_client(&sandbox, &token).await;
    attach(&leaver, &pane, None);
    let mut left = Screen::new(&pane);
    run("echo SEEN_$((6*7))", "SEEN_42").await;
    assert!(left.follow_until(&mut leaver, "SEEN_42").await);
    drop(leaver);
    let last = left.seqno.expect("the leaver applied something");

    run("echo MISSED_$((6*7))", "MISSED_42").await;
    let mut back = connect_client(&sandbox, &token).await;
    attach(&back, &pane, Some(last));
    let mut caught_up = left.resumed();
    assert!(caught_up.follow_until(&mut back, "MISSED_42").await);
    assert_eq!(
        (caught_up.snapshots, caught_up.resets),
        (0, 0),
        "diffs only"
    );
    assert!(!caught_up.updates.is_empty());
    assert!(
        caught_up.updates.iter().all(|s| *s > last),
        "{last:?}: {:?}",
        caught_up.updates
    );

    // Far more than is worth replaying: 40 repaints that flip every cell
    // between `0` and `1`, some 30 KB of diff each, against the 256 KiB past
    // which a snapshot is the cheaper catch-up. Paced, so that however the
    // daemon batches its reads, well over the 9 it takes are diffs of their own.
    let line = "l=$(printf %079d 0 | tr 0 $((i % 2)))";
    let repaint = format!("{line}; printf '\\033[H'; for r in $(seq 23); do echo $l; done");
    let flood = format!("for i in $(seq 40); do {repaint}; sleep 0.05; done");
    run(&format!("{flood}; echo FLOOD_$((6*7))"), "FLOOD_42").await;
    let mut late = connect_client(&sandbox, &token).await;
    attach(&late, &pane, Some(last));
    let mut reset = left.resumed();
    assert!(reset.follow_until(&mut late, "FLOOD_42").await);
    assert_eq!(reset.resets, 1);
    assert!(reset.snapshots >= 1);
}

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
