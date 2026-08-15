//! Cross-process integration test for session process isolation (issue #126).
//!
//! Spawns a *real* `kmuxd` with `--session-isolation process`, so each pane's
//! VT pipeline runs in an isolated `kmux-vt-worker` subprocess. It then kills a
//! worker abnormally (standing in for a libghostty-vt SIGSEGV) and asserts the
//! headline invariant of #126: **the daemon survives**, the crashed pane
//! surfaces a `PaneFaulted` to its client, and a fresh session still works.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kmux_client::connect::ConnectResult;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{
    ClientCapabilities, ClientMessage, ServerMessage, SessionEventMsg, TermSize,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;

/// Serializes the test's process-global `XDG_*` / env mutations.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pid_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn set_env(key: &str, val: &Path) {
    // SAFETY: guarded by ENV_LOCK; no other test mutates env concurrently.
    unsafe { std::env::set_var(key, val) };
}

fn set_xdg(dir: &Path) {
    for key in [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        set_env(key, dir);
    }
}

/// SIGKILLs tracked PIDs on drop so a panicking test never leaks a daemon, a
/// worker, or an orphaned shell.
#[derive(Default)]
struct Cleanup {
    pids: std::sync::Mutex<Vec<i32>>,
}

impl Cleanup {
    fn track(&self, pid: i32) {
        self.pids.lock().unwrap().push(pid);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for &pid in self.pids.lock().unwrap().iter() {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// Locate (building if necessary) the `kmux-vt-worker` binary next to the test's
/// `kmuxd`, and point the daemon at it via `KMUX_VT_WORKER_BIN`. Under
/// `cargo test --workspace` (mise run test) it is already built; under a bare
/// `cargo test -p kmuxd` we build it on demand so the test is self-contained.
fn ensure_worker_binary(kmuxd_exe: &Path) -> PathBuf {
    let worker = kmuxd_exe.with_file_name("kmux-vt-worker");
    if !worker.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "kmux-vt-worker"])
            .status()
            .expect("run cargo build -p kmux-vt-worker");
        assert!(status.success(), "failed to build kmux-vt-worker");
    }
    assert!(worker.exists(), "kmux-vt-worker not found at {worker:?}");
    worker
}

async fn wait_for_daemon() -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = kmux_client::daemon::query_daemon().await {
            return Some(status.pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn spawn_daemon(exe: &Path) -> u32 {
    let mut child = Command::new(exe)
        .args([
            "--daemon",
            "--bind",
            "127.0.0.1",
            "--port",
            "0",
            "--tcp-port",
            "0",
            "--session-isolation",
            "process",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn kmuxd");
    let _ = child.wait(); // reap the daemonize parent
    wait_for_daemon().await.expect("daemon did not come up")
}

async fn recv_until(
    rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    timeout: Duration,
    pred: impl Fn(&ServerMessage) -> bool,
) -> Option<ServerMessage> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(msg)) if pred(&msg) => return Some(msg),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}

/// A connected, authenticated client driving the data socket.
struct Client {
    tx: mpsc::UnboundedSender<ClientMessage>,
    rx: mpsc::UnboundedReceiver<ServerMessage>,
}

async fn connect_client(token: &str) -> Client {
    let (srv_tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let data_sock = kmux_sys::dirs::data_socket_path().expect("data socket path");
    let tx = match connect_uds(
        &data_sock,
        token.to_string(),
        srv_tx,
        ClientCapabilities::default(),
        None,
    )
    .await
    {
        ConnectResult::Connected(tx) => tx,
        ConnectResult::Failed(e) => panic!("UDS connect failed: {e}"),
    };
    // Answer the identity challenge, then await the successful result (issue #146).
    let auth = loop {
        match recv_until(&mut rx, Duration::from_secs(5), |m| {
            matches!(
                m,
                ServerMessage::AuthChallenge { .. } | ServerMessage::AuthResult { .. }
            )
        })
        .await
        {
            Some(ServerMessage::AuthChallenge { nonce }) => {
                assert!(kmux_client::tcp_connect::answer_auth_challenge(&tx, &nonce));
            }
            other => break other,
        }
    };
    assert!(
        matches!(auth, Some(ServerMessage::AuthResult { success: true, .. })),
        "expected a successful AuthResult"
    );
    Client { tx, rx }
}

const SIZE: TermSize = TermSize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// Create a session running a shell, then attach to its first pane. Returns the
/// pane id.
async fn create_and_attach(client: &mut Client, request_id: u64) -> String {
    client
        .tx
        .send(ClientMessage::SessionCreate {
            request_id,
            name: None,
            peer: None,
            cwd: None,
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), "exec cat".into()],
            size: SIZE,
        })
        .expect("send SessionCreate");
    let created = recv_until(&mut client.rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::SessionCreated { .. })
    })
    .await
    .expect("SessionCreated");
    let ServerMessage::SessionCreated { entry, .. } = created else {
        unreachable!()
    };
    let pane_id = format!("{}/0", entry.meta.word_id);
    client
        .tx
        .send(ClientMessage::Attach {
            pane_id: pane_id.clone(),
            last_seqno: None,
            size: SIZE,
        })
        .expect("send Attach");
    pane_id
}

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
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global env for the whole test
async fn worker_crash_is_isolated_from_the_daemon() {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    set_xdg(tmp.path());

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let worker_bin = ensure_worker_binary(&exe);
    set_env("KMUX_VT_WORKER_BIN", &worker_bin);
    // Process isolation is requested via the `--session-isolation process` flag
    // that `spawn_daemon` passes (replaces the former KMUX_SESSION_ISOLATION env).

    let cleanup = Cleanup::default();
    let daemon_pid = spawn_daemon(&exe).await;
    cleanup.track(daemon_pid as i32);

    let token = kmux_client::daemon::query_daemon()
        .await
        .expect("status")
        .token;

    // Session A, running in an isolated worker; keep the client attached.
    let mut client = connect_client(&token).await;
    let pane_a = create_and_attach(&mut client, 1).await;
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
    let status = kmux_client::daemon::query_daemon()
        .await
        .expect("daemon should still answer the control socket");
    assert_eq!(status.pid, daemon_pid, "same daemon, still serving");

    // And a brand-new isolated session still works after the crash.
    let mut client_b = connect_client(&token).await;
    let pane_b = create_and_attach(&mut client_b, 2).await;
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
