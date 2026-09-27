//! Out-of-process VT engine: the emulator runs in an isolated `kmux-vt-worker`
//! subprocess (issue #126).
//!
//! The daemon spawns one worker per pane, hands it a `dup` of the PTY master fd
//! over a socketpair, and keeps the authoritative master fd itself (so the shell
//! survives a worker crash). A **supervisor task** drains the worker's event
//! stream and fans diffs out to clients through the same
//! [`dispatch_diff_result`] the in-process
//! relay uses — so a worker pane is byte-identical on the wire to an in-process
//! one. A daemon-side [`CellGrid`] mirror, fed from that same stream, answers
//! `snapshot()` synchronously (no IPC round-trip), which keeps the existing
//! synchronous attach/resize call sites unchanged.
//!
//! Nothing about a worker may hang the daemon (issue #207). The worker must
//! answer `Hello` within [`READY_TIMEOUT`]; its child handle is a
//! `tokio::process::Child`, waited on asynchronously, never with a blocking
//! `wait` on a runtime thread; a stream that goes bad gets the worker killed
//! before it is reaped; and a worker that has sent nothing for
//! [`NO_PROGRESS_DEADLINE`] while its PTY holds unread output is taken for
//! hung and killed. Each of those ends counts as a crash, so the pane is
//! faulted and its worker respawned.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{
    CursorState, GridSnapshot, ScrollbackLine, ServerMessage, SessionEventMsg, TermModes, TermSize,
};
use kmux_pty::process::ExitStatus;
use kmux_pty::registry::SessionManager;
use kmux_worker_protocol::{
    ChildExitStatus, WORKER_PROTOCOL_VERSION, WorkerEvent, WorkerRequest, codec,
};
use tokio::io::AsyncRead;
use tokio::net::UnixStream;
use tokio::process::Child;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::{INPUT_QUEUE_CAPACITY, PaneInput, QueueRejected, rejected};
use crate::app::{ClientMap, PaneEventSink};
use crate::backend::{BackendEventSink, ControlEvent};
use crate::diff_engine::DiffResult;
use crate::relay::dispatch_diff_result;
use crate::scrollback::DiffBuffer;

/// Env var overriding the worker binary path. Normally the daemon finds
/// `kmux-vt-worker` next to its own executable; tests and packagers can override.
const WORKER_BIN_ENV: &str = "KMUX_VT_WORKER_BIN";

/// Longest a freshly spawned worker may take to answer `Hello` with `Ready`.
/// A worker that does not is killed and the pane falls back to in-process.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a quiet worker is checked for a hang.
const LIVENESS_INTERVAL: Duration = Duration::from_secs(5);

/// A worker that has sent nothing for this long while its PTY has output
/// waiting is hung: a healthy one drains the PTY and emits a diff for every
/// read. Output that never stops arriving keeps a healthy worker talking, and
/// a quiet PTY gives a healthy worker nothing to say, so neither trips it.
const NO_PROGRESS_DEADLINE: Duration = Duration::from_secs(30);

/// How long a worker that closed its event stream gets to exit on its own
/// before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// Events buffered between the task reading the worker's socket and the
/// supervisor applying them.
const EVENT_BUFFER: usize = 64;

/// Daemon-side handle to one pane's isolated VT worker.
pub struct WorkerEngine {
    /// Outbound control requests (resize, capabilities, shutdown); drained by
    /// the writer task onto the socket ahead of queued input.
    req_tx: mpsc::UnboundedSender<WorkerRequest>,
    /// Client input for the worker, bounded like the in-process pane's queue:
    /// a worker whose PTY stopped taking input refuses more (issue #206).
    input_tx: mpsc::Sender<WorkerRequest>,
    /// Mirror of the worker's grid, fed from the event stream, so `snapshot()`
    /// and history reads stay synchronous on the daemon side.
    mirror: Arc<Mutex<CellGrid>>,
    /// OS pid of the `kmux-vt-worker` subprocess (distinct from the shell pid the
    /// worker adopts). Captured at spawn so status reporting can surface it; the
    /// `Child` itself is owned by the supervisor task for reaping.
    child_pid: u32,
    /// Supervisor task: reads worker events, fans out, reaps the child. When
    /// it is aborted (pane close, handoff) the `Child` it owned is dropped
    /// unreaped and tokio reaps it in the background.
    supervisor: JoinHandle<()>,
    /// Writer task: drains `req_tx` and `input_tx` onto the socket.
    writer_task: JoinHandle<()>,
}

/// Everything the supervisor needs to fan a worker's output out to clients —
/// the same shared handles the in-process relay loop holds.
pub struct WorkerFanout {
    pub pane_id: String,
    pub clients: ClientMap,
    pub scrollback: Arc<Mutex<DiffBuffer>>,
    pub seqno_counter: Arc<AtomicU64>,
    pub event_sink: Arc<PaneEventSink>,
    pub manager: Arc<SessionManager>,
    /// Reports this pane id for respawn when the worker crashes (issue #126).
    pub fault_tx: mpsc::UnboundedSender<String>,
}

impl WorkerEngine {
    /// Spawn a worker for `pane_id`, hand it `master_fd` (a dup of the PTY
    /// master), complete the version handshake, and start the supervisor.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        pid: i32,
        size: TermSize,
        scrollback_lines: u32,
        kitty_graphics: bool,
        kitty_keyboard: bool,
        master_fd: OwnedFd,
        fanout: WorkerFanout,
    ) -> anyhow::Result<Self> {
        let (daemon_end, worker_end) =
            std::os::unix::net::UnixStream::pair().context("worker socketpair")?;
        let worker_raw = worker_end.as_raw_fd();

        // A dup of the master kept by the supervisor, to see whether the PTY
        // has output the worker has not read (the hang check).
        let pty_probe = master_fd
            .try_clone()
            .context("dup PTY master for probing")?;

        let exe = resolve_worker_exe()?;
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.env("KMUX_WORKER_SOCKET_FD", WORKER_SOCKET_FD.to_string());
        // SAFETY: `inherit_as` is async-signal-safe and touches only the raw
        // socket fd we own.
        unsafe {
            cmd.pre_exec(move || inherit_as(worker_raw, WORKER_SOCKET_FD));
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn worker binary {}", exe.display()))?;
        drop(worker_end); // the parent no longer needs the worker end

        daemon_end
            .set_nonblocking(true)
            .context("worker socket nonblocking")?;
        let stream = UnixStream::from_std(daemon_end).context("adopt worker socket")?;

        // Handshake: Hello (carrying the PTY fd) -> Ready. A worker that
        // fails it, or never answers, is killed and reaped here.
        let hello = WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION,
            pane_id: fanout.pane_id.clone(),
            pid,
            size,
            scrollback: scrollback_lines,
            kitty_graphics,
            kitty_keyboard,
        };
        if let Err(e) = handshake(&stream, &hello, master_fd, READY_TIMEOUT).await {
            let _ = child.kill().await;
            return Err(e);
        }

        let mirror = Arc::new(Mutex::new(CellGrid::new(
            size.rows.max(1) as usize,
            size.cols.max(1) as usize,
        )));

        let (sock_rd, mut sock_wr) = stream.into_split();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<WorkerRequest>();
        let (input_tx, mut input_rx) = mpsc::channel::<WorkerRequest>(INPUT_QUEUE_CAPACITY);

        let writer_task = tokio::spawn(async move {
            loop {
                // Control first: a resize or shutdown is not queued behind
                // input still waiting on the daemon's side. (Once on the
                // socket, the worker handles requests in order, so one it is
                // stuck writing to a child that does not read still delays
                // what follows it.)
                let req = tokio::select! {
                    biased;
                    req = req_rx.recv() => req,
                    req = input_rx.recv() => req,
                };
                let Some(req) = req else { break };
                if let Err(e) = codec::send_msg(&mut sock_wr, &req).await {
                    debug!("worker request writer stopping: {e}");
                    break;
                }
            }
        });

        // Capture the worker pid before the `Child` moves into the supervisor.
        let child_pid = child.id().unwrap_or_default();
        let supervisor = tokio::spawn(supervise(sock_rd, child, pty_probe, mirror.clone(), fanout));

        Ok(Self {
            req_tx,
            input_tx,
            mirror,
            child_pid,
            supervisor,
            writer_task,
        })
    }

    /// OS pid of the worker subprocess, for status reporting.
    pub(super) fn child_pid(&self) -> u32 {
        self.child_pid
    }

    pub(super) fn snapshot(&self) -> GridSnapshot {
        self.mirror.lock().unwrap().to_snapshot()
    }

    pub(super) fn resize_emulator(&self, size: TermSize) {
        // Resize the mirror viewport now (blanks it; the worker's repaint diff
        // refills it) and tell the worker to resize its emulator.
        self.mirror.lock().unwrap().resize(size.rows, size.cols);
        let _ = self.req_tx.send(WorkerRequest::Resize { size });
    }

    pub(super) fn checkpoint_grid(&self, max_lines: usize) -> (GridSnapshot, Vec<ScrollbackLine>) {
        let mirror = self.mirror.lock().unwrap();
        let grid = mirror.to_snapshot();
        let sb = mirror.scrollback();
        let total = sb.history_total();
        let want = (max_lines as u64).min(total);
        let start = total - want;
        let mut lines = Vec::with_capacity(want as usize);
        for abs in start..total {
            match sb.get_absolute(abs) {
                Some(line) => lines.push(line.clone()),
                None => break,
            }
        }
        (grid, lines)
    }

    pub(super) fn mirror_range_and_total(
        &self,
        start: u64,
        count: u32,
    ) -> (u64, Vec<ScrollbackLine>, u64) {
        let mirror = self.mirror.lock().unwrap();
        let sb = mirror.scrollback();
        let history_total = sb.history_total();
        let first = start.max(sb.base_index());
        let mut lines = Vec::new();
        let mut abs = first;
        while lines.len() < count as usize {
            match sb.get_absolute(abs) {
                Some(line) => {
                    lines.push(line.clone());
                    abs += 1;
                }
                None => break,
            }
        }
        (first, lines, history_total)
    }

    pub(super) fn enqueue_input(&self, input: PaneInput) -> Result<(), QueueRejected> {
        let req = match input {
            PaneInput::Bytes(data) => WorkerRequest::Input { data },
            PaneInput::Keys(events) => WorkerRequest::Keys { events },
            PaneInput::Paste(data) => WorkerRequest::Paste { data },
        };
        self.input_tx.try_send(req).map_err(|e| rejected(&e))
    }

    pub(super) fn set_capabilities(&self, kitty_graphics: bool, kitty_keyboard: bool) {
        let _ = self.req_tx.send(WorkerRequest::SetCapabilities {
            kitty_graphics,
            kitty_keyboard,
        });
    }

    pub(super) fn abort_relay_task(&mut self) -> JoinHandle<()> {
        // Ask the worker to exit cleanly (releasing its PTY dup), then stop our
        // tasks. The shell survives because the daemon holds the master fd.
        let _ = self.req_tx.send(WorkerRequest::Shutdown);
        self.writer_task.abort();
        self.supervisor.abort();
        std::mem::replace(&mut self.supervisor, tokio::spawn(async {}))
    }
}

/// Send `Hello` with the PTY master fd and wait up to `timeout` for a `Ready`
/// carrying this build's protocol version. The master is closed on our side
/// once sent: the worker holds its own dup.
async fn handshake(
    stream: &UnixStream,
    hello: &WorkerRequest,
    master_fd: OwnedFd,
    timeout: Duration,
) -> anyhow::Result<()> {
    codec::send_with_fd(stream, hello, Some(master_fd.as_raw_fd()))
        .await
        .context("send Hello")?;
    drop(master_fd);
    let (ready, _fd) = tokio::time::timeout(timeout, codec::recv_with_fd::<WorkerEvent>(stream))
        .await
        .map_err(|_| anyhow::anyhow!("worker sent no Ready within {timeout:?}"))?
        .context("recv Ready")?;
    match ready {
        WorkerEvent::Ready { version } if version == WORKER_PROTOCOL_VERSION => Ok(()),
        WorkerEvent::Ready { version } => {
            anyhow::bail!(
                "worker protocol mismatch: worker={version}, daemon={WORKER_PROTOCOL_VERSION}"
            )
        }
        other => anyhow::bail!("expected Ready from worker, got {other:?}"),
    }
}

/// How a worker's event stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamEnd {
    /// The worker closed it: it is exiting.
    Closed,
    /// A frame failed to read or decode: the worker is still running but
    /// its stream can no longer be trusted.
    Corrupt,
    /// The worker went quiet while its PTY held unread output.
    Hung,
}

/// Read the worker's event stream, fan it out to clients, and reap the child.
///
/// When the worker dies abnormally (a SIGSEGV in libghostty-vt, or any non-zero
/// exit) — as opposed to the clean exit triggered by a pane close or handoff —
/// the daemon is unaffected (this is just a task seeing EOF); we surface a
/// [`SessionEventMsg::PaneFaulted`] to attached clients so the crash is visible.
/// A corrupt stream or a hang is ended the same way, by killing the worker.
/// The shell survives because the daemon still holds the PTY master fd.
async fn supervise(
    sock_rd: impl AsyncRead + Unpin + Send + 'static,
    mut child: Child,
    pty_probe: OwnedFd,
    mirror: Arc<Mutex<CellGrid>>,
    fanout: WorkerFanout,
) {
    let output_waiting = || output_waiting(pty_probe.as_fd());
    let end = apply_events(sock_rd, &mirror, &fanout, output_waiting, LIVENESS_INTERVAL).await;
    if end_worker(&mut child, end, &fanout.pane_id, EXIT_GRACE).await {
        warn!(pane_id = %fanout.pane_id, ?end, "isolated VT worker crashed; surfacing fault (daemon and other sessions unaffected)");
        broadcast_fault(&fanout);
        // Ask the daemon to respawn the worker (the shell is still alive). If the
        // respawn channel is gone (shutdown), the pane simply stays faulted.
        let _ = fanout.fault_tx.send(fanout.pane_id.clone());
    }
}

/// Apply the worker's events until its stream ends or it hangs, checking
/// every `check_every` whether it has gone quiet while `output_waiting` says
/// its PTY has output for it.
///
/// The socket is read by a task of its own feeding a bounded channel, so the
/// periodic check never cancels a frame half-read.
async fn apply_events(
    mut sock_rd: impl AsyncRead + Unpin + Send + 'static,
    mirror: &Arc<Mutex<CellGrid>>,
    fanout: &WorkerFanout,
    output_waiting: impl Fn() -> bool,
    check_every: Duration,
) -> StreamEnd {
    let (tx, mut rx) = mpsc::channel(EVENT_BUFFER);
    let reader = tokio::spawn(async move {
        loop {
            let frame = codec::recv_msg::<_, WorkerEvent>(&mut sock_rd).await;
            let more = matches!(frame, Ok(Some(_)));
            if tx.send(frame).await.is_err() {
                break;
            }
            if !more {
                break;
            }
        }
    });
    let _reader = crate::supervisor::AbortOnDrop(reader);

    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;
    let mut last_heard = tokio::time::Instant::now();
    let mut check = tokio::time::interval(check_every);
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Some(Ok(Some(ev))) => {
                    last_heard = tokio::time::Instant::now();
                    handle_event(ev, mirror, fanout, &mut prev_cursor, &mut prev_modes);
                }
                Some(Ok(None)) | None => {
                    debug!(pane_id = %fanout.pane_id, "worker closed its event stream");
                    return StreamEnd::Closed;
                }
                Some(Err(e)) => {
                    warn!(pane_id = %fanout.pane_id, "worker event read error: {e}");
                    return StreamEnd::Corrupt;
                }
            },
            _ = check.tick() => {
                if is_hung(last_heard.elapsed(), output_waiting()) {
                    return StreamEnd::Hung;
                }
            }
        }
    }
}

/// Whether a worker that has been quiet for `quiet_for` is hung, given
/// whether its PTY has output waiting (see [`NO_PROGRESS_DEADLINE`]).
fn is_hung(quiet_for: Duration, output_waiting: bool) -> bool {
    output_waiting && quiet_for >= NO_PROGRESS_DEADLINE
}

/// Whether the PTY behind `master` (a dup of its master) has output nobody
/// has read yet: readable, and not merely at end of file.
///
/// A zero-timeout `poll`, since `FIONREAD` on a PTY master reads 0 on macOS.
/// Once the pane's program has exited the master reports `POLLHUP` (with
/// `POLLIN` on some systems) for good, and a worker with nothing left to do
/// must not look hung, so a hang-up is not output. A failed `poll` is no
/// output either.
fn output_waiting(master: BorrowedFd<'_>) -> bool {
    use nix::libc::{POLLHUP, POLLIN, poll, pollfd};

    let mut probe = pollfd {
        fd: master.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the call; the fd is borrowed and outlives it.
    let ready = unsafe { poll(&raw mut probe, 1, 0) };
    // Only POLLIN was asked for, so a ready fd without a hang-up has output.
    ready == 1 && probe.revents & POLLHUP == 0
}

/// Reap the worker (the daemon's direct child) so it does not linger as a
/// zombie, and report whether it died abnormally. A clean exit (code 0) is the
/// pane-close / handoff path; a signal death (SIGSEGV/SIGABRT) or non-zero exit
/// is a crash that should fault the pane.
///
/// A worker whose stream went bad or which hung is still running, so it is
/// killed first; one that closed its stream gets `grace` ([`EXIT_GRACE`]) to
/// exit before it is killed too. The wait is asynchronous: it never blocks a
/// runtime thread, however the worker ended.
async fn end_worker(child: &mut Child, end: StreamEnd, pane_id: &str, grace: Duration) -> bool {
    if end != StreamEnd::Closed {
        warn!(pane_id, ?end, "killing isolated VT worker");
        let _ = child.start_kill();
    }
    let status = match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            warn!(pane_id, "isolated VT worker did not exit; killing it");
            let _ = child.start_kill();
            child.wait().await
        }
    };
    match status {
        Ok(status) => {
            let faulted = !status.success();
            debug!(pane_id, ?status, faulted, "worker reaped");
            faulted
        }
        Err(e) => {
            warn!(pane_id, "worker wait failed: {e}");
            false
        }
    }
}

/// Tell attached clients the pane's worker crashed. Uses the control lane so
/// the notice is never dropped (same channel `PaneEventSink` uses).
fn broadcast_fault(fanout: &WorkerFanout) {
    let msg = ServerMessage::Event {
        event: SessionEventMsg::PaneFaulted {
            pane_id: fanout.pane_id.clone(),
        },
    };
    for sender in fanout.clients.lock().unwrap().values() {
        let _ = sender.ctrl_tx.send(msg.clone());
    }
}

/// Apply one worker event: update the mirror and fan out to clients via the
/// shared dispatch (identical to the in-process path), or forward a backend
/// event through the pane's sink.
fn handle_event(
    ev: WorkerEvent,
    mirror: &Arc<Mutex<CellGrid>>,
    fanout: &WorkerFanout,
    prev_cursor: &mut CursorState,
    prev_modes: &mut TermModes,
) {
    let snapshot_fn = || mirror.lock().unwrap().to_snapshot();
    match ev {
        WorkerEvent::Ready { .. } => {}
        WorkerEvent::Diff {
            diff,
            scrollback_lines,
        } => {
            // Update the mirror first so a force-full-snapshot client served
            // mid-fan-out sees this frame. apply_diff handles scrollback_reset;
            // append the new lines after so a reset can't drop them.
            {
                let mut m = mirror.lock().unwrap();
                let first_index = diff
                    .history_total
                    .saturating_sub(scrollback_lines.len() as u64);
                m.apply_diff(diff.clone());
                if !scrollback_lines.is_empty() {
                    m.apply_scrollback_append(first_index, scrollback_lines.clone());
                }
            }
            dispatch_diff_result(
                &fanout.pane_id,
                DiffResult::CellDiff {
                    diff,
                    scrollback_lines,
                },
                0,
                &fanout.scrollback,
                &fanout.clients,
                &fanout.seqno_counter,
                prev_cursor,
                prev_modes,
                &snapshot_fn,
            );
        }
        WorkerEvent::CursorOnly {
            cursor,
            modes,
            history_total,
        } => {
            mirror.lock().unwrap().apply_cursor_update(cursor, modes);
            dispatch_diff_result(
                &fanout.pane_id,
                DiffResult::CursorOnly {
                    cursor,
                    modes,
                    history_total,
                },
                0,
                &fanout.scrollback,
                &fanout.clients,
                &fanout.seqno_counter,
                prev_cursor,
                prev_modes,
                &snapshot_fn,
            );
        }
        // Re-emit worker → daemon events through the same single dispatch the
        // in-process path uses (issue #187).
        WorkerEvent::Title { title } => {
            fanout
                .event_sink
                .on_control_event(ControlEvent::Title(&title));
        }
        WorkerEvent::Bell => fanout.event_sink.on_control_event(ControlEvent::Bell),
        WorkerEvent::Osc52 {
            selection,
            base64_data,
        } => fanout.event_sink.on_control_event(ControlEvent::Osc52Copy {
            selection: &selection,
            base64_data: &base64_data,
        }),
        WorkerEvent::ChildExit { status } => {
            fanout
                .manager
                .notify_exited(&fanout.pane_id, to_exit_status(status));
        }
        WorkerEvent::Fault { detail } => {
            warn!(pane_id = %fanout.pane_id, "worker reported fault: {detail}");
        }
        // The daemon answers snapshot/history from its mirror, so it never asks
        // the worker; ignore any unsolicited response.
        WorkerEvent::Snapshot { .. } | WorkerEvent::History { .. } => {}
    }
}

fn to_exit_status(status: ChildExitStatus) -> ExitStatus {
    match status {
        ChildExitStatus::Code(c) => ExitStatus::Code(c),
        ChildExitStatus::Signal(s) => ExitStatus::Signal(s),
        ChildExitStatus::Unknown => ExitStatus::Unknown,
    }
}

/// The fd number the worker reads its socket from (`KMUX_WORKER_SOCKET_FD`).
const WORKER_SOCKET_FD: RawFd = 3;

/// In a forked child before `exec`: make `fd` survive `exec` as `target`.
///
/// `dup2` onto another number yields a copy without `FD_CLOEXEC`. When `fd`
/// already *is* `target`, `dup2` does nothing and leaves the flag set (Rust
/// opens every fd close-on-exec), so the program would start without it; the
/// descriptor flags are cleared explicitly instead.
///
/// Async-signal-safe: only `fcntl`, `dup2` and reading `errno`.
fn inherit_as(fd: RawFd, target: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl/dup2 on an fd the caller owns; a failure is -1 plus errno.
    let rc = unsafe {
        if fd == target {
            nix::libc::fcntl(fd, nix::libc::F_SETFD, 0)
        } else {
            nix::libc::dup2(fd, target)
        }
    };
    if rc == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Locate the `kmux-vt-worker` binary: `$KMUX_VT_WORKER_BIN`, else next to the
/// running daemon, else fall back to the bare name on `PATH`.
fn resolve_worker_exe() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var(WORKER_BIN_ENV) {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("current_exe")?;
    // After an in-place upgrade the path may be suffixed " (deleted)"; strip it.
    let dir = exe
        .parent()
        .map(|d| {
            let s = d.to_string_lossy();
            PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s).to_string())
        })
        .context("daemon executable has no parent directory")?;
    let candidate = dir.join("kmux-vt-worker");
    if candidate.exists() {
        Ok(candidate)
    } else {
        Ok(PathBuf::from("kmux-vt-worker"))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;

    use super::*;

    /// Fd numbers the probe works with. They must be single digits: dash
    /// (`/bin/sh` on Debian and Ubuntu) only parses fds 0-9 in a redirection,
    /// so a probe of a higher number fails whatever the fd's state.
    const PROBE_SRC: RawFd = 8;
    const PROBE_TARGET: RawFd = 9;

    /// Signature of `inherit_as`, so the probe can also run a no-op control.
    type Inherit = fn(RawFd, RawFd) -> std::io::Result<()>;

    /// In the forked child: put a close-on-exec copy of stdout at `src`, as the
    /// socketpair end is close-on-exec, and close whatever the test harness
    /// may have left open at `target`, so only `inherit` can open it.
    ///
    /// Async-signal-safe: only `dup2`, `fcntl` and `close`.
    fn place_cloexec(src: RawFd, target: RawFd) -> std::io::Result<()> {
        // SAFETY: fd juggling in the child's own table; a failure is -1 plus
        // errno. Closing an fd that is not open only yields EBADF, ignored.
        unsafe {
            if src != target {
                nix::libc::close(target);
            }
            if nix::libc::dup2(nix::libc::STDOUT_FILENO, src) == -1
                || nix::libc::fcntl(src, nix::libc::F_SETFD, nix::libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Whether `target` is open in a program exec'd after the child placed a
    /// close-on-exec fd at `src` and ran `inherit(src, target)`. A real fork
    /// and exec, because what is under test is the close-on-exec flag `exec`
    /// honours (R7).
    fn target_open_after_exec(src: RawFd, target: RawFd, inherit: Inherit) -> bool {
        let mut cmd = std::process::Command::new("/bin/sh");
        // `: >&N` fails unless fd N is open in the shell.
        cmd.args(["-c", &format!(": >&{target}")])
            .stderr(std::process::Stdio::null());
        // SAFETY: `place_cloexec` and `inherit` are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                place_cloexec(src, target)?;
                inherit(src, target)
            });
        }
        cmd.status().expect("run sh").success()
    }

    /// Input is mapped onto its worker request, in order, and a full input
    /// queue refuses more instead of waiting.
    #[tokio::test]
    async fn enqueue_input_maps_each_input_and_refuses_a_full_queue() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (input_tx, mut input_rx) = mpsc::channel(3);
        let engine = WorkerEngine {
            req_tx,
            input_tx,
            mirror: Arc::new(Mutex::new(CellGrid::new(1, 1))),
            child_pid: 0,
            supervisor: tokio::spawn(async {}),
            writer_task: tokio::spawn(async {}),
        };
        engine
            .enqueue_input(PaneInput::Bytes(b"b".to_vec()))
            .unwrap();
        engine.enqueue_input(PaneInput::Keys(vec![])).unwrap();
        engine
            .enqueue_input(PaneInput::Paste(b"p".to_vec()))
            .unwrap();
        assert!(matches!(
            engine.enqueue_input(PaneInput::Bytes(vec![])),
            Err(mpsc::error::TrySendError::Full(()))
        ));

        assert!(matches!(input_rx.try_recv(), Ok(WorkerRequest::Input { data }) if data == b"b"));
        assert!(
            matches!(input_rx.try_recv(), Ok(WorkerRequest::Keys { events }) if events.is_empty())
        );
        assert!(matches!(input_rx.try_recv(), Ok(WorkerRequest::Paste { data }) if data == b"p"));
    }

    /// A stand-in worker process: `sh -c script`, spawned as the daemon
    /// spawns a worker, so it is reaped the same way.
    fn fixture_worker(script: &str) -> Child {
        tokio::process::Command::new("/bin/sh")
            .args(["-c", script])
            .spawn()
            .expect("spawn a stand-in worker")
    }

    /// The fan-out a worker pane owns, with the title its events update and
    /// the channel its faults are reported on.
    fn fixture_fanout() -> (
        WorkerFanout,
        Arc<Mutex<String>>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let title = Arc::new(Mutex::new(String::new()));
        let (fault_tx, fault_rx) = mpsc::unbounded_channel();
        let fanout = WorkerFanout {
            pane_id: "eagle/0".to_string(),
            clients: Arc::default(),
            scrollback: Arc::new(Mutex::new(DiffBuffer::new(1024))),
            seqno_counter: Arc::new(AtomicU64::new(1)),
            event_sink: Arc::new(PaneEventSink::new(
                "eagle/0".to_string(),
                Arc::clone(&title),
                Arc::default(),
                tokio::sync::broadcast::channel(8).0,
            )),
            manager: Arc::new(SessionManager::new()),
            fault_tx,
        };
        (fanout, title, fault_rx)
    }

    fn fixture_mirror() -> Arc<Mutex<CellGrid>> {
        Arc::new(Mutex::new(CellGrid::new(1, 1)))
    }

    /// A bound on waits for a real process; the code under test has its own.
    const GUARD: Duration = Duration::from_secs(10);

    /// A worker that answers `Hello` with a matching `Ready` passes the
    /// handshake; one of another protocol version, or one that answers
    /// something else, fails it.
    #[tokio::test]
    async fn the_handshake_wants_a_ready_of_this_protocol_version() {
        let hello = WorkerRequest::Shutdown;
        let answers = [
            (
                WorkerEvent::Ready {
                    version: WORKER_PROTOCOL_VERSION,
                },
                true,
            ),
            (
                WorkerEvent::Ready {
                    version: WORKER_PROTOCOL_VERSION + 1,
                },
                false,
            ),
            (WorkerEvent::Bell, false),
        ];
        for (answer, accepted) in answers {
            let (daemon, worker) = UnixStream::pair().unwrap();
            codec::send_with_fd(&worker, &answer, None).await.unwrap();
            let fd = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
            let shook = handshake(&daemon, &hello, fd, GUARD).await;
            assert_eq!(shook.is_ok(), accepted, "{answer:?}: {shook:?}");
        }
    }

    /// A worker that never answers `Hello` fails the handshake at the
    /// timeout instead of hanging pane creation (issue #207).
    #[tokio::test(start_paused = true)]
    async fn a_worker_that_never_sends_ready_times_out() {
        let (daemon, _silent_worker) = UnixStream::pair().unwrap();
        let fd = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let started = tokio::time::Instant::now();

        let shook = handshake(&daemon, &WorkerRequest::Shutdown, fd, READY_TIMEOUT).await;

        let err = shook.expect_err("no Ready");
        assert!(err.to_string().contains("no Ready"), "{err}");
        assert!(started.elapsed() >= READY_TIMEOUT);
    }

    /// A worker that sends a garbage frame is killed and reaped, the pane is
    /// reported faulted for a respawn, and the supervisor returns: nothing
    /// waits on the still-running worker (issue #207).
    #[tokio::test]
    async fn a_worker_sending_a_garbage_frame_is_killed_reaped_and_faulted() {
        let (daemon, mut worker) = UnixStream::pair().unwrap();
        let child = fixture_worker("sleep 30");
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id().unwrap()).unwrap());
        let (fanout, _title, mut faults) = fixture_fanout();
        let probe = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        // A well-framed payload that is not a `WorkerEvent`.
        kmux_protocol::codec::write_frame(&mut worker, &[0xff, 0xff, 0xff])
            .await
            .unwrap();

        let (sock_rd, _sock_wr) = daemon.into_split();
        tokio::time::timeout(
            GUARD,
            supervise(sock_rd, child, probe, fixture_mirror(), fanout),
        )
        .await
        .expect("the supervisor returns without waiting on the worker");

        assert_eq!(faults.try_recv().as_deref(), Ok("eagle/0"));
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH),
            "killed and reaped"
        );
    }

    /// Every event before the stream closes is applied, in order, and a
    /// closed stream ends the supervision as `Closed`.
    #[tokio::test]
    async fn events_are_applied_until_the_stream_closes() {
        let (daemon, mut worker) = UnixStream::pair().unwrap();
        let (fanout, title, _faults) = fixture_fanout();
        for name in ["first", "second"] {
            let ev = WorkerEvent::Title {
                title: name.to_string(),
            };
            codec::send_msg(&mut worker, &ev).await.unwrap();
        }
        drop(worker);

        let end = apply_events(daemon, &fixture_mirror(), &fanout, || false, GUARD).await;

        assert_eq!(end, StreamEnd::Closed);
        assert_eq!(*title.lock().unwrap(), "second");
    }

    /// A worker that goes quiet while its PTY holds unread output is taken
    /// for hung once the no-progress deadline passes; with nothing to read,
    /// a quiet worker is left alone.
    #[tokio::test(start_paused = true)]
    async fn a_quiet_worker_is_hung_only_while_output_waits() {
        let (fanout, _title, _faults) = fixture_fanout();
        let (daemon, _quiet_worker) = UnixStream::pair().unwrap();
        let started = tokio::time::Instant::now();
        let end = apply_events(
            daemon,
            &fixture_mirror(),
            &fanout,
            || true,
            LIVENESS_INTERVAL,
        )
        .await;
        assert_eq!(end, StreamEnd::Hung);
        assert!(started.elapsed() >= NO_PROGRESS_DEADLINE);

        let (daemon, _quiet_worker) = UnixStream::pair().unwrap();
        let idle = tokio::time::timeout(
            NO_PROGRESS_DEADLINE * 3,
            apply_events(
                daemon,
                &fixture_mirror(),
                &fanout,
                || false,
                LIVENESS_INTERVAL,
            ),
        )
        .await;
        assert!(idle.is_err(), "an idle worker is not hung");
    }

    #[test]
    fn a_worker_is_hung_after_the_deadline_with_output_waiting() {
        let just_under = NO_PROGRESS_DEADLINE - Duration::from_millis(1);
        let cases = [
            (NO_PROGRESS_DEADLINE, true, true),
            (NO_PROGRESS_DEADLINE * 2, true, true),
            (just_under, true, false),
            (NO_PROGRESS_DEADLINE, false, false),
        ];
        for (quiet_for, waiting, hung) in cases {
            assert_eq!(is_hung(quiet_for, waiting), hung, "{quiet_for:?} {waiting}");
        }
    }

    /// A PTY master shows output waiting once the program wrote some nobody
    /// read, and none once that output is read or the program has exited. A
    /// real PTY, because the readiness is the kernel's (R7).
    #[tokio::test]
    async fn output_waiting_reads_the_pty_masters_readiness() {
        let (_session, mut reader, writer) =
            crate::fixtures::fixture_pty("printf ready; read line; exit 0").await;
        let fd = reader.as_raw_fd();
        // SAFETY: the reader keeps the fd open for the whole test.
        let master = unsafe { BorrowedFd::borrow_raw(fd) };
        let until = |want: bool| async move {
            while output_waiting(master) != want {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        tokio::time::timeout(GUARD, until(true))
            .await
            .expect("the output shows up");
        let mut buf = [0u8; 64];
        let n = reader.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ready");
        assert!(!output_waiting(master), "read, so nothing waits");

        // Let the program exit; its echo is read away, and the hang-up that
        // follows is not output.
        writer.write_all(b"\n").await.unwrap();
        tokio::time::timeout(GUARD, async {
            while reader.read(&mut buf).await.is_ok_and(|n| n > 0) {}
        })
        .await
        .expect("the program exits");
        assert!(!output_waiting(master), "a hang-up is not output");
    }

    /// A worker whose stream went bad is killed rather than waited on; one
    /// that closed its stream is waited on and, exiting cleanly, is not a
    /// fault; one that closed its stream but never exits is killed after the
    /// grace period.
    #[tokio::test]
    async fn ending_a_worker_kills_it_unless_it_closed_its_stream() {
        let mut running = fixture_worker("sleep 30");
        assert!(end_worker(&mut running, StreamEnd::Corrupt, "eagle/0", GUARD).await);

        let mut hung = fixture_worker("sleep 30");
        assert!(end_worker(&mut hung, StreamEnd::Hung, "eagle/0", GUARD).await);

        let mut exiting = fixture_worker("sleep 0.2");
        assert!(!end_worker(&mut exiting, StreamEnd::Closed, "eagle/0", GUARD).await);

        let mut lingering = fixture_worker("sleep 30");
        let grace = Duration::from_millis(50);
        assert!(end_worker(&mut lingering, StreamEnd::Closed, "eagle/0", grace).await);
    }

    /// Guards the probe: without `inherit_as` a close-on-exec fd must read as
    /// closed, or the tests below could pass vacuously.
    #[test]
    fn probe_reports_a_cloexec_fd_as_closed() {
        let open = target_open_after_exec(PROBE_TARGET, PROBE_TARGET, |_, _| Ok(()));
        assert!(!open, "fd {PROBE_TARGET} survived exec despite FD_CLOEXEC");
    }

    /// The latent bug: a socket already at the target number was `dup2`ed onto
    /// itself, a no-op that left it close-on-exec.
    #[test]
    fn inherit_as_passes_an_fd_already_at_the_target_number() {
        let open = target_open_after_exec(PROBE_TARGET, PROBE_TARGET, inherit_as);
        assert!(open, "fd {PROBE_TARGET} closed on exec");
    }

    #[test]
    fn inherit_as_passes_an_fd_under_a_new_number() {
        let open = target_open_after_exec(PROBE_SRC, PROBE_TARGET, inherit_as);
        assert!(open, "fd {PROBE_TARGET} not open");
    }

    /// A failed `fcntl`/`dup2` surfaces its errno, so `spawn` fails instead of
    /// exec'ing a worker without its socket. Fd -1 is never open, so both calls
    /// fail with EBADF without touching any fd in this process.
    #[test]
    fn inherit_as_reports_a_bad_fd() {
        for target in [-1, PROBE_TARGET] {
            let err = inherit_as(-1, target).expect_err("fd -1 is not open");
            assert_eq!(
                err.raw_os_error(),
                Some(nix::libc::EBADF),
                "target {target}"
            );
        }
    }
}
