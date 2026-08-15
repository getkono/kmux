//! End-to-end regression for the terminal query-response path.
//!
//! Full-screen and interactive programs send terminal queries (DSR/DA/…) and
//! block until the emulator replies. Before this path existed, kmux parsed those
//! queries but never wrote a reply back to the child, so programs stalled until a
//! timeout or the next keypress (delayed `vim :q` repaint, invisible `fzf`).
//!
//! This test drives the real daemon with a child that emits `CSI 6 n` (DSR
//! cursor-position report), reads exactly the 6-byte reply back from its stdin,
//! and echoes it visibly via `cat -v`. If the reply never arrives the child
//! blocks forever and the test times out; when it works, `^[[1;1R` appears on
//! the grid — proving the query → reply → child round-trip completes with **no**
//! user input. It runs under both the in-process and process-isolated engines,
//! since the fix must behave identically across that seam.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use kmux_client::connect::ConnectResult;
use kmux_client::grid::CellGrid;
use kmux_client::tcp_connect::connect_uds;
use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ServerMessage, TermSize};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::sync::mpsc;

/// Serializes the test's process-global `XDG_*` / env mutations.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

/// SIGKILLs tracked PIDs on drop so a panicking test never leaks a daemon.
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

/// Spawn a daemon; `isolation` is `Some("process")` for the worker engine or
/// `None` for the default in-process engine.
async fn spawn_daemon(exe: &Path, isolation: Option<&str>) -> u32 {
    let mut args = vec![
        "--daemon",
        "--bind",
        "127.0.0.1",
        "--port",
        "0",
        "--tcp-port",
        "0",
    ];
    if let Some(mode) = isolation {
        args.push("--session-isolation");
        args.push(mode);
    }
    let mut child = Command::new(exe)
        .args(&args)
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

/// Create a session running `program`, then attach to its first pane.
async fn create_and_attach(client: &mut Client, request_id: u64, program: &[&str]) -> String {
    client
        .tx
        .send(ClientMessage::SessionCreate {
            request_id,
            name: None,
            peer: None,
            cwd: None,
            program: Some(program[0].into()),
            args: program[1..].iter().map(|s| (*s).into()).collect(),
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

/// Reconstruct the pane's grid from the daemon's messages into one flat string,
/// applying updates until `pred` matches the accumulated text or `timeout`.
async fn grid_text_until(
    client: &mut Client,
    pane_id: &str,
    timeout: Duration,
    pred: impl Fn(&str) -> bool,
) -> String {
    let mut grid = CellGrid::new(SIZE.rows as usize, SIZE.cols as usize);
    let deadline = Instant::now() + timeout;
    loop {
        let text: String = grid.to_snapshot().cells.iter().map(|c| c.c).collect();
        if pred(&text) {
            return text;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return text;
        }
        let Ok(Some(msg)) = tokio::time::timeout(remaining, client.rx.recv()).await else {
            return grid.to_snapshot().cells.iter().map(|c| c.c).collect();
        };
        match msg {
            ServerMessage::TerminalSnapshot {
                pane_id: p,
                snapshot,
                ..
            } if p == pane_id => {
                grid.apply_snapshot((*snapshot).clone());
            }
            ServerMessage::TerminalUpdate {
                pane_id: p, diff, ..
            } if p == pane_id => {
                grid.apply_diff((*diff).clone());
            }
            ServerMessage::CursorUpdate {
                pane_id: p,
                cursor,
                modes,
                ..
            } if p == pane_id => grid.apply_cursor_update(cursor, modes),
            _ => {}
        }
    }
}

/// The DSR cursor-position round-trip: the child emits `CSI 6 n`, reads the
/// 6-byte reply the daemon writes back, and echoes it via `cat -v` as `^[[1;1R`.
/// A missing reply blocks the child forever and this times out.
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global env for the whole test
async fn assert_dsr_roundtrip(isolation: Option<&str>) {
    let _env = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    set_xdg(tmp.path());

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_kmuxd"));
    if isolation.is_some() {
        let worker_bin = ensure_worker_binary(&exe);
        set_env("KMUX_VT_WORKER_BIN", &worker_bin);
    }

    let cleanup = Cleanup::default();
    let daemon_pid = spawn_daemon(&exe, isolation).await;
    cleanup.track(daemon_pid as i32);

    let token = kmux_client::daemon::query_daemon()
        .await
        .expect("status")
        .token;

    let mut client = connect_client(&token).await;
    // Emit DSR, read back exactly the 6-byte `\x1b[1;1R` reply, echo it visibly.
    let pane = create_and_attach(
        &mut client,
        1,
        &["/bin/sh", "-c", "printf '\\033[6n'; head -c 6 | cat -v"],
    )
    .await;

    let text = grid_text_until(&mut client, &pane, Duration::from_secs(10), |t| {
        t.contains("[1;1R")
    })
    .await;

    assert!(
        text.contains("[1;1R"),
        "the DSR cursor-position reply must round-trip back to the child and \
         render (looked for `^[[1;1R` via `cat -v`); grid was: {:?}",
        text.trim_end()
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global env for the whole test
async fn dsr_query_reply_reaches_child_in_process() {
    assert_dsr_roundtrip(None).await;
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK guards process-global env for the whole test
async fn dsr_query_reply_reaches_child_isolated_worker() {
    assert_dsr_roundtrip(Some("process")).await;
}
