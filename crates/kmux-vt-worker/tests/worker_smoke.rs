//! End-to-end smoke test for the isolated VT worker.
//!
//! Drives a real `kmux-vt-worker` subprocess exactly as kmuxd will: spawn a PTY
//! (the "daemon" keeps the authoritative master fd), hand the worker a `dup` of
//! that fd over a socketpair via `SCM_RIGHTS`, then exchange protocol frames.
//! Proves the handshake, fd adoption, and that PTY output becomes diffs.

use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use kmux_protocol::messages::TermSize;
use kmux_pty::PtyProcess;
use kmux_pty::config::PtyConfig;
use kmux_worker_protocol::{WORKER_PROTOCOL_VERSION, WorkerEvent, WorkerRequest, codec};
use tokio::net::UnixStream;

/// A pane running in a real worker subprocess turns PTY output into cell diffs:
/// `cat` echoes the input we write, the PTY surfaces it, and the worker emits a
/// non-empty `Diff`. This exercises the whole boundary — fd passing, handshake,
/// the steady-state stream — end to end.
#[tokio::test]
async fn worker_processes_pty_and_emits_diff() {
    // The "daemon" owns the PTY; `cat` echoes stdin straight back to stdout.
    let pty = PtyProcess::spawn(&PtyConfig::new("/bin/cat")).expect("spawn pty");
    // Don't let our drop SIGKILL the child out from under the worker.
    pty.set_keep_alive(true);
    let pid = pty.pid.as_raw();
    let size = TermSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    let master_dup = pty.io.dup_owned().expect("dup master fd");

    // Socketpair: the worker end is handed to the child on fd 3.
    let (daemon_end, worker_end) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let worker_raw = worker_end.as_raw_fd();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kmux-vt-worker"));
    cmd.env("KMUX_WORKER_SOCKET_FD", "3");
    // SAFETY: dup2 is async-signal-safe; we only touch the raw fd we own.
    unsafe {
        cmd.pre_exec(move || {
            if nix::libc::dup2(worker_raw, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn worker");
    drop(worker_end); // parent no longer needs the worker end

    daemon_end.set_nonblocking(true).expect("nonblocking");
    let stream = UnixStream::from_std(daemon_end).expect("tokio stream");

    // Handshake: send Hello carrying the PTY master fd; expect Ready.
    codec::send_with_fd(
        &stream,
        &WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION,
            pane_id: "eagle/0".into(),
            pid,
            size,
            scrollback: 1000,
            kitty_graphics: false,
            kitty_keyboard: false,
        },
        Some(master_dup.as_raw_fd()),
    )
    .await
    .expect("send Hello");
    let (ready, _fd) = codec::recv_with_fd::<WorkerEvent>(&stream)
        .await
        .expect("recv Ready");
    assert!(
        matches!(ready, WorkerEvent::Ready { version } if version == WORKER_PROTOCOL_VERSION),
        "expected Ready, got {ready:?}"
    );
    drop(master_dup); // the worker holds its own dup now

    // Steady state: write input and expect a non-empty cell diff back.
    let (mut rd, mut wr) = stream.into_split();
    codec::send_msg(
        &mut wr,
        &WorkerRequest::Input {
            data: b"hello\n".to_vec(),
        },
    )
    .await
    .expect("send Input");

    let mut got_diff = false;
    for _ in 0..50 {
        match tokio::time::timeout(
            Duration::from_secs(5),
            codec::recv_msg::<_, WorkerEvent>(&mut rd),
        )
        .await
        {
            Ok(Ok(Some(WorkerEvent::Diff { diff, .. }))) if !diff.ops.is_empty() => {
                got_diff = true;
                break;
            }
            Ok(Ok(Some(_))) => continue, // Title / CursorOnly / empty diff
            _ => break,
        }
    }
    assert!(
        got_diff,
        "worker should emit a non-empty cell diff after input echoes through the PTY"
    );

    // Clean shutdown; reap the worker and the shell.
    let _ = codec::send_msg(&mut wr, &WorkerRequest::Shutdown).await;
    let _ = child.wait();
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
}
