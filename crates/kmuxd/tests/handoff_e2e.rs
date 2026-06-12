//! Cross-process integration tests for the graceful daemon handoff that backs the
//! live daemon upgrade (issues #35 / #36).
//!
//! These spawn a *real* `kmuxd` (via `CARGO_BIN_EXE_kmuxd`), open the data socket
//! to create a session with a long-lived shell, trigger `restart` over the control
//! socket, and assert that a successor process takes over with the running shell
//! intact. Unlike the in-process `live_pty_migrates_with_same_pid` unit test (which
//! hand-transfers an fd between two `ServerApp`s in one process), they exercise the
//! actual fork / exec / daemonize / `SCM_RIGHTS` path — including the in-place
//! binary swap that `just upgrade-daemon` performs.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kmux_client::connect::ConnectResult;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ServerMessage, TermSize};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;

/// Serializes the tests: each mutates process-global `XDG_*` env vars (so the dirs
/// helpers and the spawned daemon agree on socket paths). Held for the whole test.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pid_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

/// Point the current process's XDG dirs at an isolated temp dir. Spawned daemons
/// inherit these; the kmux-client control helpers read them to resolve sockets.
/// Caller must hold [`ENV_LOCK`].
fn set_xdg(dir: &Path) {
    for key in [
        "XDG_RUNTIME_DIR",
        "XDG_STATE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
    ] {
        // SAFETY: guarded by ENV_LOCK; no other test mutates env concurrently.
        unsafe { std::env::set_var(key, dir) };
    }
}

/// SIGKILLs every tracked PID on drop so a panicking test never leaks a daemon or
/// an orphaned shell.
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

/// Spawn `exe` as a background daemon (isolated dirs already set) and return the
/// PID the control socket reports once it is up. Binds loopback so a restricted CI
/// network namespace can't block startup.
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
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn kmuxd");
    // The daemonize parent forks the real daemon and exits immediately; reap it so
    // it doesn't linger as a zombie. The daemon itself is reparented to init and is
    // tracked via the control socket / pid file, not this handle.
    let _ = child.wait();
    wait_for_daemon(None)
        .await
        .expect("daemon did not come up within the deadline")
}

/// Poll the control socket until a live daemon whose PID differs from `exclude` is
/// reported. Returns its PID, or `None` on timeout.
async fn wait_for_daemon(exclude: Option<u32>) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = kmux_client::daemon::query_daemon().await
            && Some(status.pid) != exclude
        {
            return Some(status.pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Await `f` becoming true (sync predicate), polling until `timeout`.
async fn poll_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Receive from `rx` until a message matches `pred` or `timeout` elapses.
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

/// Block until `path` holds a parseable PID (the shell writes its own), or time out.
fn read_pid_file(path: &Path, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<i32>()
        {
            return Some(pid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Connect over the data UDS, create a session whose initial pane runs a shell that
/// records its own PID to `pidfile`, then drop the client. The session persists
/// server-side. Returns the shell's PID.
async fn create_session_with_recorded_child(token: &str, cwd: &Path, pidfile: &Path) -> i32 {
    let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let data_sock = kmux_protocol::dirs::data_socket_path().expect("data socket path");
    let client_tx = match connect_uds(
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

    let auth = recv_until(&mut srv_rx, Duration::from_secs(5), |m| {
        matches!(m, ServerMessage::AuthResult { success: true, .. })
    })
    .await;
    assert!(auth.is_some(), "expected a successful AuthResult");

    // `exec sleep` so the recorded PID *is* the long-lived process the handoff must
    // keep alive (no intermediate `sh` that could exit and change the PID).
    let script = format!("echo $$ > {}; exec sleep 600", pidfile.display());
    client_tx
        .send(ClientMessage::SessionCreate {
            request_id: 1,
            name: Some("e2e".into()),
            cwd: Some(cwd.display().to_string()),
            program: Some("/bin/sh".into()),
            args: vec!["-c".into(), script],
            size: TermSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
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
/// daemon — exercising spawn_successor → SCM_RIGHTS → restore_with_handoff.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global XDG vars for the whole test
async fn live_restart_preserves_running_shell_across_processes() {
    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_xdg(tmp.path());
    let cleanup = Cleanup::default();

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    let old_pid = spawn_daemon(&exe).await;
    cleanup.track(old_pid as i32);

    let token = kmux_client::daemon::query_daemon()
        .await
        .expect("status")
        .token;
    let pidfile = tmp.path().join("child.pid");
    let child = create_session_with_recorded_child(&token, tmp.path(), &pidfile).await;
    cleanup.track(child);
    assert!(pid_alive(child), "shell should be alive before the restart");
    assert!(
        kmux_client::daemon::query_daemon()
            .await
            .unwrap()
            .session_count
            >= 1,
        "the session should be present before the restart"
    );

    let accepted = kmux_client::daemon::restart_daemon()
        .await
        .expect("restart control request");
    assert!(accepted, "daemon should accept the graceful handoff");

    let new_pid = wait_for_daemon(Some(old_pid))
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
        kmux_client::daemon::query_daemon()
            .await
            .unwrap()
            .session_count
            >= 1,
        "the session must persist across the restart"
    );

    let _ = kmux_client::daemon::stop_daemon().await;
}

/// B2: replacing the daemon binary in place (as `cargo install` does) before
/// `restart` still hands off. Regression guard for `resolve_successor_exe`: on Linux
/// the atomic rename unlinks the running inode, so `current_exe()` reads back as
/// `"<path> (deleted)"` — re-execing that literal path would ENOENT and silently
/// keep the old code running. Passes trivially on macOS (no marker); the assertion
/// has teeth on Linux.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn in_place_binary_swap_still_hands_off() {
    use std::os::unix::fs::PermissionsExt;

    let _env = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    set_xdg(tmp.path());
    let cleanup = Cleanup::default();

    // Run from a writable copy so we can replace it in place mid-flight.
    let exe = tmp.path().join("kmuxd");
    std::fs::copy(env!("CARGO_BIN_EXE_kmuxd"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

    let old_pid = spawn_daemon(&exe).await;
    cleanup.track(old_pid as i32);
    let token = kmux_client::daemon::query_daemon()
        .await
        .expect("status")
        .token;
    let pidfile = tmp.path().join("child.pid");
    let child = create_session_with_recorded_child(&token, tmp.path(), &pidfile).await;
    cleanup.track(child);

    // Simulate `cargo install`'s atomic replace: stage a fresh copy and rename it
    // over the running binary (unlinking the running inode on Linux).
    let staged = tmp.path().join("kmuxd.new");
    std::fs::copy(env!("CARGO_BIN_EXE_kmuxd"), &staged).unwrap();
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&staged, &exe).unwrap();

    let accepted = kmux_client::daemon::restart_daemon()
        .await
        .expect("restart control request");
    assert!(accepted, "daemon should accept the graceful handoff");

    let new_pid = wait_for_daemon(Some(old_pid))
        .await
        .expect("a successor must take over even after an in-place binary swap");
    cleanup.track(new_pid as i32);
    assert_ne!(new_pid, old_pid, "the successor must have a distinct PID");
    assert!(
        pid_alive(child),
        "the running shell must survive an in-place daemon upgrade"
    );

    let _ = kmux_client::daemon::stop_daemon().await;
}
