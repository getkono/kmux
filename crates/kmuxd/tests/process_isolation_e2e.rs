//! Cross-process integration test for session process isolation (issue #126).
//!
//! Spawns a *real* `kmuxd` with `--session-isolation process`, so each pane's
//! VT pipeline runs in an isolated `kmux-vt-worker` subprocess, and kills
//! workers abnormally (standing in for a libghostty-vt SIGSEGV). A worker is a
//! real process, which is why this cannot run in-process; the restart budget's
//! windowing is unit-tested on its own in `app/recover.rs`.

#![cfg(unix)]

mod harness;

use harness::{
    Cleanup, Client, Daemon, E2E_TIMEOUT, Sandbox, connect_client, create_and_attach, daemon_token,
    recv_until, wait_for,
};
use kmux_protocol::control_rpc::WorkerInfo;
use kmux_protocol::messages::{ServerMessage, SessionEventMsg};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// How many times a crashed worker is respawned within its window:
/// `kmuxd::app::recover::MAX_RESTARTS`.
const MAX_RESTARTS: usize = 3;

/// The daemon's report on `pane`'s worker, once it names a worker other than
/// `not`: a respawn, or the first worker when `not` is 0.
async fn worker_of(sandbox: &Sandbox, pane: &str, not: u32) -> Option<WorkerInfo> {
    wait_for(E2E_TIMEOUT, || async {
        kmux_client::daemon::query_workers_at(&sandbox.socket_path())
            .await
            .ok()?
            .workers
            .into_iter()
            .find(|w| w.pane_id == pane && w.worker_pid != not)
    })
    .await
}

/// Kill `worker` as a crash would, and wait for `client` to be told its pane
/// faulted.
async fn crash(worker: &WorkerInfo, client: &mut Client) {
    let pid = Pid::from_raw(worker.worker_pid.cast_signed());
    kill(pid, Signal::SIGKILL).expect("kill the worker");
    let faulted = recv_until(&mut client.rx, E2E_TIMEOUT, |m| {
        matches!(m, ServerMessage::Event { event: SessionEventMsg::PaneFaulted { pane_id } }
            if *pane_id == worker.pane_id)
    })
    .await;
    assert!(faulted.is_some(), "{} did not fault", worker.pane_id);
}

/// A snapshot of `pane` reaches `client`: an attach, or a resync.
async fn snapshot_of(client: &mut Client, pane: &str) -> bool {
    recv_until(
        &mut client.rx,
        E2E_TIMEOUT,
        |m| matches!(m, ServerMessage::TerminalSnapshot { pane_id, .. } if pane_id == pane),
    )
    .await
    .is_some()
}

/// A crashed worker faults only its own pane: the daemon lives on, respawns
/// the worker and resyncs the pane's client — up to [`MAX_RESTARTS`] times.
/// Past that the pane is left faulted, while another pane's worker still
/// recovers. This is the acceptance test for #126.
#[tokio::test]
async fn a_crashing_worker_is_respawned_up_to_its_budget_then_left_faulted() {
    let sandbox = Sandbox::new();
    let cleanup = Cleanup::default();
    let daemon_pid = Daemon::new(&sandbox).isolated().spawn(None).await;
    cleanup.track(daemon_pid.cast_signed());
    let token = daemon_token(&sandbox).await;
    let mut client = connect_client(&sandbox, &token).await;
    let pane = create_and_attach(&mut client, 1, None).await;
    assert!(snapshot_of(&mut client, &pane).await, "attached");

    let mut worker = worker_of(&sandbox, &pane, 0).await.expect("a worker");
    for respawns in 1..=MAX_RESTARTS {
        cleanup.track(worker.worker_pid.cast_signed());
        crash(&worker, &mut client).await;
        worker = worker_of(&sandbox, &pane, worker.worker_pid)
            .await
            .unwrap_or_else(|| panic!("respawn {respawns}:\n{}", sandbox.daemon_log()));
        assert_eq!(worker.restart_count, respawns);
        assert!(snapshot_of(&mut client, &pane).await, "resync {respawns}");
    }
    assert!(!worker.within_restart_budget);
    crash(&worker, &mut client).await;

    // Faults are handled one at a time, in order, so once a crash in another
    // pane has been answered with a respawn, this one's has been decided.
    let mut other = connect_client(&sandbox, &token).await;
    let other_pane = create_and_attach(&mut other, 2, None).await;
    assert!(snapshot_of(&mut other, &other_pane).await, "attached");
    let other_worker = worker_of(&sandbox, &other_pane, 0).await.expect("a worker");
    cleanup.track(other_worker.worker_pid.cast_signed());
    crash(&other_worker, &mut other).await;
    let respawned = worker_of(&sandbox, &other_pane, other_worker.worker_pid)
        .await
        .expect("the other pane recovers");
    cleanup.track(respawned.worker_pid.cast_signed());

    let left = worker_of(&sandbox, &pane, 0).await.expect("still listed");
    assert_eq!(
        (left.worker_pid, left.restart_count),
        (worker.worker_pid, MAX_RESTARTS),
        "no respawn past the budget"
    );
    let status = kmux_client::daemon::query_daemon_at(&sandbox.socket_path()).await;
    assert_eq!(
        status.map(|s| s.pid),
        Some(daemon_pid),
        "the daemon survives"
    );
}
