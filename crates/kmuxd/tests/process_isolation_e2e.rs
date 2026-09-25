//! Cross-process integration test for session process isolation (issue #126).
//!
//! Spawns a *real* `kmuxd` with `--session-isolation process`, so each pane's
//! VT pipeline runs in an isolated `kmux-vt-worker` subprocess. It then kills a
//! worker abnormally (standing in for a libghostty-vt SIGSEGV) and asserts the
//! headline invariant of #126: **the daemon survives**, the crashed pane
//! surfaces a `PaneFaulted` to its client, and a fresh session still works.

#![cfg(unix)]

mod harness;

use std::process::Command;
use std::time::{Duration, Instant};

use harness::{
    Cleanup, Daemon, Sandbox, connect_client, create_and_attach, daemon_token, pid_alive,
    recv_until,
};
use kmux_protocol::messages::{ServerMessage, SessionEventMsg};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// Find the worker subprocess that is a child of `daemon_pid`.
fn find_worker_pid(daemon_pid: u32, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(out) = Command::new("pgrep")
            .args(["-P", &daemon_pid.to_string(), "kmux-vt-worker"])
            .output()
            && let Some(pid) = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .and_then(|l| l.trim().parse::<i32>().ok())
        {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A worker SIGSEGV (here a SIGKILL standing in for any abnormal death) faults
/// only its own pane: the client is told `PaneFaulted`, the daemon stays alive,
/// and a brand-new session still works. This is the acceptance test for #126.
#[tokio::test]
async fn worker_crash_is_isolated_from_the_daemon() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    let daemon_pid = Daemon::new(&sandbox).isolated().spawn(None).await;
    cleanup.track(daemon_pid as i32);

    let token = daemon_token(&sandbox).await;

    // Session A, running in an isolated worker; keep the client attached.
    let mut client = connect_client(&sandbox, &token).await;
    let pane_a = create_and_attach(&mut client, 1, None).await;
    // The attach replays a snapshot minted from the daemon-side mirror, proving
    // the worker pane is live end-to-end through the daemon.
    let snap = recv_until(
        &mut client.rx,
        Duration::from_secs(5),
        |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == pane_a),
    )
    .await;
    assert!(
        snap.is_some(),
        "attach should replay a snapshot for the worker pane"
    );

    // Find and abnormally kill the pane's worker.
    let worker_pid = find_worker_pid(daemon_pid, Duration::from_secs(5))
        .expect("the isolated worker subprocess should be running");
    cleanup.track(worker_pid);
    assert!(
        pid_alive(worker_pid),
        "worker should be alive before the kill"
    );
    kill(Pid::from_raw(worker_pid), Signal::SIGKILL).expect("kill worker");

    // The client is told its pane faulted (not that the daemon died).
    let faulted = recv_until(&mut client.rx, Duration::from_secs(10), |m| {
        matches!(
            m,
            ServerMessage::Event {
                event: SessionEventMsg::PaneFaulted { pane_id }
            } if *pane_id == pane_a
        )
    })
    .await;
    assert!(
        faulted.is_some(),
        "the crashed pane should surface PaneFaulted to its client"
    );

    // Self-healing: the shell is still alive, so the daemon respawns the worker
    // and resyncs the client to the fresh emulator with a snapshot.
    let resync = recv_until(
        &mut client.rx,
        Duration::from_secs(10),
        |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == pane_a),
    )
    .await;
    assert!(
        resync.is_some(),
        "the faulted pane should respawn its worker and resync the client"
    );
    assert!(
        find_worker_pid(daemon_pid, Duration::from_secs(5)).is_some(),
        "a replacement worker should be running for the recovered pane"
    );

    // Headline: the daemon is still alive and responsive.
    assert!(
        pid_alive(daemon_pid as i32),
        "daemon must survive a worker crash"
    );
    let status = kmux_client::daemon::query_daemon_at(&sandbox.socket_path())
        .await
        .expect("daemon should still answer the control socket");
    assert_eq!(status.pid, daemon_pid, "same daemon, still serving");

    // And a brand-new isolated session still works after the crash.
    let mut client_b = connect_client(&sandbox, &token).await;
    let pane_b = create_and_attach(&mut client_b, 2, None).await;
    let snap_b = recv_until(
        &mut client_b.rx,
        Duration::from_secs(5),
        |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if *pane_id == pane_b),
    )
    .await;
    assert!(
        snap_b.is_some(),
        "a fresh isolated session must work after another worker crashed"
    );
    if let Some(b) = find_worker_pid(daemon_pid, Duration::from_secs(5)) {
        cleanup.track(b);
    }
}
