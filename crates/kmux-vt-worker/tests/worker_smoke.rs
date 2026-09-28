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

fn is_nonempty_diff(ev: &WorkerEvent) -> bool {
    matches!(ev, WorkerEvent::Diff { diff, .. } if !diff.ops.is_empty())
}

/// The next event `wanted` picks out, skipping the rest; `None` when the
/// stream ends or none comes within five seconds of the last.
async fn next_event(
    rd: &mut tokio::net::unix::OwnedReadHalf,
    wanted: impl Fn(&WorkerEvent) -> bool,
) -> Option<WorkerEvent> {
    loop {
        let ev = tokio::time::timeout(
            Duration::from_secs(5),
            codec::recv_msg::<_, WorkerEvent>(rd),
        )
        .await
        .ok()?
        .ok()??;
        if wanted(&ev) {
            return Some(ev);
        }
    }
}

/// A real worker subprocess with the first half of the handshake done: it has
/// adopted the PTY and answered `Hello` with `Ready`.
struct Started {
    worker: std::process::Child,
    stream: UnixStream,
    /// SIGKILLs the worker and the shell however the test ends: a starved
    /// worker never sees its socket close.
    _cleanup: KillOnDrop,
}

/// Spawn a worker for `pty` exactly as kmuxd does — the "daemon" keeps the
/// authoritative master fd and hands the worker a `dup` over a socketpair —
/// and exchange `Hello` for `Ready`.
async fn start_worker(pty: &PtyProcess) -> Started {
    // Don't let our drop SIGKILL the child out from under the worker.
    pty.set_keep_alive(true);
    let pid = pty.pid.as_raw();
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
    let worker = cmd.spawn().expect("spawn worker");
    drop(worker_end); // parent no longer needs the worker end
    let cleanup = KillOnDrop(vec![worker.id().cast_signed(), pid]);

    daemon_end.set_nonblocking(true).expect("nonblocking");
    let stream = UnixStream::from_std(daemon_end).expect("tokio stream");

    codec::send_with_fd(
        &stream,
        &WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION,
            pane_id: "eagle/0".into(),
            pid,
            size: TermSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            scrollback: 1000,
            kitty_graphics: false,
            kitty_keyboard: false,
        },
        Some(master_dup.as_raw_fd()),
    )
    .await
    .expect("send Hello");
    // `master_dup` drops here: the worker holds its own copy now.
    let (ready, _fd) = tokio::time::timeout(
        Duration::from_secs(10),
        codec::recv_with_fd::<WorkerEvent>(&stream),
    )
    .await
    .expect("Ready within ten seconds")
    .expect("recv Ready");
    assert!(
        matches!(ready, WorkerEvent::Ready { version } if version == WORKER_PROTOCOL_VERSION),
        "expected Ready, got {ready:?}"
    );
    Started {
        worker,
        stream,
        _cleanup: cleanup,
    }
}

/// A worker reads nothing until the daemon has accepted its `Ready`: one the
/// daemon abandons there (its `Ready` came too late) leaves every byte the
/// pane printed in the PTY, for the in-process engine that takes the pane
/// over. It used to start reading the moment it sent `Ready`, so what it read
/// before the daemon killed it was lost. Out of process because what is under
/// test is when the worker binary itself first reads its PTY.
#[tokio::test]
async fn a_worker_abandoned_after_ready_leaves_the_output_unread() {
    const PRINTED: &[u8] = b"PRINTED_BEFORE_THE_WORKER";
    let pty = PtyProcess::spawn(
        &PtyConfig::new("/bin/sh").args(["-c", "printf PRINTED_BEFORE_THE_WORKER; exec sleep 60"]),
    )
    .expect("spawn pty");
    let master = pty.io.as_raw_fd();
    assert!(
        readable_within(master, Duration::from_secs(10)),
        "the pane printed"
    );

    let Started {
        mut worker,
        stream,
        _cleanup,
    } = start_worker(&pty).await;
    // The daemon gives up on the worker: it closes its end without `Start`.
    let (mut rd, wr) = stream.into_split();
    drop(wr);
    let reported = next_event(&mut rd, is_nonempty_diff).await;
    assert!(reported.is_none(), "an unconfirmed worker reports nothing");
    let exited = reap_within(&mut worker, Duration::from_secs(10)).await;
    assert!(exited.is_some(), "an abandoned worker exits");

    let mut unread = Vec::new();
    let mut buf = [0u8; 256];
    while unread.len() < PRINTED.len() && readable_within(master, Duration::from_secs(5)) {
        match pty.io.try_read_raw(&mut buf) {
            Ok(n) if n > 0 => unread.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    assert_eq!(
        String::from_utf8_lossy(&unread),
        String::from_utf8_lossy(PRINTED),
        "what the pane printed is still in the PTY"
    );
}

/// Whether `fd` has input within `timeout`.
fn readable_within(fd: std::os::fd::RawFd, timeout: Duration) -> bool {
    use nix::libc::{POLLIN, poll, pollfd};
    let mut fds = [pollfd {
        fd,
        events: POLLIN,
        revents: 0,
    }];
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid `pollfd` for an fd the test keeps open.
    unsafe { poll(fds.as_mut_ptr(), 1, ms) > 0 }
}

/// A pane running in a real worker subprocess turns PTY output into cell diffs:
/// `cat` echoes the input we write, the PTY surfaces it, and the worker emits a
/// non-empty `Diff`. This exercises the whole boundary — fd passing, handshake,
/// the steady-state stream — end to end.
#[tokio::test]
async fn worker_processes_pty_and_emits_diff() {
    // The "daemon" owns the PTY; `cat` echoes stdin straight back to stdout.
    let pty = PtyProcess::spawn(&PtyConfig::new("/bin/cat")).expect("spawn pty");
    let pid = pty.pid.as_raw();
    let Started {
        worker: mut child,
        stream,
        _cleanup,
    } = start_worker(&pty).await;

    // Keep the worker, then write input and expect a non-empty cell diff back.
    let (mut rd, mut wr) = stream.into_split();
    codec::send_msg(&mut wr, &WorkerRequest::Start)
        .await
        .expect("send Start");
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

    heartbeat_and_hold(&mut rd, &mut wr)
        .await
        .expect("heartbeat and hold");

    // The shell exiting is reported: the PTY's EOF is the only exit signal
    // for a child the worker cannot `waitpid`.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    let exit = next_event(&mut rd, |ev| matches!(ev, WorkerEvent::ChildExit { .. })).await;
    assert!(exit.is_some(), "the child's exit is reported");

    // Clean shutdown; reap the worker and the shell.
    // Bounded: a worker that ignored `Shutdown` must fail the test, not hang it.
    let _ = codec::send_msg(&mut wr, &WorkerRequest::Shutdown).await;
    let exited = reap_within(&mut child, Duration::from_secs(10)).await;
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    assert!(
        exited.is_some_and(|status| status.success()),
        "the worker exits cleanly on Shutdown: {exited:?}"
    );
}

/// Heartbeat (issue #207): a ping is answered with its own sequence number;
/// a hold parks the reader until released, and can be taken again.
async fn heartbeat_and_hold(
    rd: &mut tokio::net::unix::OwnedReadHalf,
    wr: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    codec::send_msg(wr, &WorkerRequest::Ping { seq: 41 }).await?;
    let pong = next_event(rd, |ev| matches!(ev, WorkerEvent::Pong { .. })).await;
    assert!(
        matches!(pong, Some(WorkerEvent::Pong { seq: 41 })),
        "{pong:?}"
    );

    // A hold parks the reader: it says so, and the pane's output waits in the
    // PTY until the hold is released.
    codec::send_msg(wr, &WorkerRequest::Hold { id: 7 }).await?;
    let held = next_event(rd, |ev| matches!(ev, WorkerEvent::Held { .. })).await;
    assert!(
        matches!(held, Some(WorkerEvent::Held { id: 7 })),
        "{held:?}"
    );
    let input = WorkerRequest::Input {
        data: b"held\n".to_vec(),
    };
    codec::send_msg(wr, &input).await?;
    // Nothing is read while held, and the worker says `Held` once, not
    // again and again.
    let read_or_held_again =
        |ev: &WorkerEvent| is_nonempty_diff(ev) || matches!(ev, WorkerEvent::Held { .. });
    let while_held = tokio::time::timeout(
        Duration::from_millis(500),
        next_event(rd, read_or_held_again),
    )
    .await;
    assert!(while_held.is_err(), "nothing while held: {while_held:?}");
    codec::send_msg(wr, &WorkerRequest::Release).await?;
    assert!(
        next_event(rd, is_nonempty_diff).await.is_some(),
        "read once released"
    );
    // And it can be held again, for a later handoff.
    codec::send_msg(wr, &WorkerRequest::Hold { id: 8 }).await?;
    let held = next_event(rd, |ev| matches!(ev, WorkerEvent::Held { .. })).await;
    assert!(
        matches!(held, Some(WorkerEvent::Held { id: 8 })),
        "{held:?}"
    );
    codec::send_msg(wr, &WorkerRequest::Release).await
}

/// Wait up to `timeout` for `child` to exit, killing it if it has not.
async fn reap_within(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// SIGKILLs these pids when dropped, however the test ends. Killing one
/// already gone is harmless.
struct KillOnDrop(Vec<i32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for &pid in &self.0 {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}
