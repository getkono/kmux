//! Out-of-process VT engine: the emulator runs in an isolated `kmux-vt-worker`
//! subprocess (issue #126).
//!
//! The daemon spawns one worker per pane, hands it a `dup` of the PTY master fd
//! over a socketpair, and keeps the authoritative master fd itself (so the shell
//! survives a worker crash). A **supervisor task** drains the worker's event
//! stream and fans diffs out to clients through the same
//! [`dispatch_diff_result`](crate::relay::dispatch_diff_result) the in-process
//! relay uses — so a worker pane is byte-identical on the wire to an in-process
//! one. A daemon-side [`CellGrid`] mirror, fed from that same stream, answers
//! `snapshot()` synchronously (no IPC round-trip), which keeps the existing
//! synchronous attach/resize call sites unchanged.

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use kmux_client::grid::CellGrid;
use kmux_protocol::messages::{
    CursorState, GridSnapshot, KeyEvent, ScrollbackLine, ServerMessage, SessionEventMsg, TermModes,
    TermSize,
};
use kmux_pty::error::Result;
use kmux_pty::process::ExitStatus;
use kmux_pty::registry::SessionManager;
use kmux_worker_protocol::{
    ChildExitStatus, WORKER_PROTOCOL_VERSION, WorkerEvent, WorkerRequest, codec,
};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::app::{ClientMap, PaneEventSink};
use crate::backend::BackendEventSink;
use crate::diff_engine::DiffResult;
use crate::relay::dispatch_diff_result;
use crate::scrollback::DiffBuffer;

/// Env var overriding the worker binary path. Normally the daemon finds
/// `kmux-vt-worker` next to its own executable; tests and packagers can override.
const WORKER_BIN_ENV: &str = "KMUX_VT_WORKER_BIN";

/// Daemon-side handle to one pane's isolated VT worker.
pub struct WorkerEngine {
    /// Outbound requests; drained by the writer task onto the socket.
    req_tx: mpsc::UnboundedSender<WorkerRequest>,
    /// Mirror of the worker's grid, fed from the event stream, so `snapshot()`
    /// and history reads stay synchronous on the daemon side.
    mirror: Arc<Mutex<CellGrid>>,
    /// Supervisor task: reads worker events, fans out, reaps the child.
    supervisor: JoinHandle<()>,
    /// Writer task: drains `req_tx` onto the socket.
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
    ) -> anyhow::Result<WorkerEngine> {
        let (daemon_end, worker_end) =
            std::os::unix::net::UnixStream::pair().context("worker socketpair")?;
        let worker_raw = worker_end.as_raw_fd();

        let exe = resolve_worker_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.env("KMUX_WORKER_SOCKET_FD", "3");
        // SAFETY: dup2 is async-signal-safe; we only touch the raw socket fd we
        // own. The child reads the socket from fd 3.
        unsafe {
            cmd.pre_exec(move || {
                if nix::libc::dup2(worker_raw, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn worker binary {}", exe.display()))?;
        drop(worker_end); // the parent no longer needs the worker end

        daemon_end
            .set_nonblocking(true)
            .context("worker socket nonblocking")?;
        let stream = UnixStream::from_std(daemon_end).context("adopt worker socket")?;

        // Handshake: Hello (carrying the PTY fd) -> Ready.
        codec::send_with_fd(
            &stream,
            &WorkerRequest::Hello {
                version: WORKER_PROTOCOL_VERSION,
                pane_id: fanout.pane_id.clone(),
                pid,
                size,
                scrollback: scrollback_lines,
                kitty_graphics,
                kitty_keyboard,
            },
            Some(master_fd.as_raw_fd()),
        )
        .await
        .context("send Hello")?;
        drop(master_fd); // the worker holds its own dup now
        let (ready, _fd) = codec::recv_with_fd::<WorkerEvent>(&stream)
            .await
            .context("recv Ready")?;
        match ready {
            WorkerEvent::Ready { version } if version == WORKER_PROTOCOL_VERSION => {}
            WorkerEvent::Ready { version } => {
                anyhow::bail!(
                    "worker protocol mismatch: worker={version}, daemon={WORKER_PROTOCOL_VERSION}"
                )
            }
            other => anyhow::bail!("expected Ready from worker, got {other:?}"),
        }

        let mirror = Arc::new(Mutex::new(CellGrid::new(
            size.rows.max(1) as usize,
            size.cols.max(1) as usize,
        )));

        let (sock_rd, mut sock_wr) = stream.into_split();
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<WorkerRequest>();

        let writer_task = tokio::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                if let Err(e) = codec::send_msg(&mut sock_wr, &req).await {
                    debug!("worker request writer stopping: {e}");
                    break;
                }
            }
        });

        let supervisor = tokio::spawn(supervise(sock_rd, child, mirror.clone(), fanout));

        Ok(WorkerEngine {
            req_tx,
            mirror,
            supervisor,
            writer_task,
        })
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

    pub(super) async fn write_input(&self, data: &[u8]) -> Result<()> {
        let _ = self.req_tx.send(WorkerRequest::Input {
            data: data.to_vec(),
        });
        Ok(())
    }

    pub(super) async fn write_keys(&self, events: &[KeyEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let _ = self.req_tx.send(WorkerRequest::Keys {
            events: events.to_vec(),
        });
        Ok(())
    }

    pub(super) async fn write_paste(&self, data: &[u8]) -> Result<()> {
        let _ = self.req_tx.send(WorkerRequest::Paste {
            data: data.to_vec(),
        });
        Ok(())
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

/// Read the worker's event stream, fan it out to clients, and reap the child.
///
/// When the worker dies abnormally (a SIGSEGV in libghostty-vt, or any non-zero
/// exit) — as opposed to the clean exit triggered by a pane close or handoff —
/// the daemon is unaffected (this is just a task seeing EOF); we surface a
/// [`SessionEventMsg::PaneFaulted`] to attached clients so the crash is visible.
/// The shell survives because the daemon still holds the PTY master fd.
async fn supervise(
    mut sock_rd: OwnedReadHalf,
    child: std::process::Child,
    mirror: Arc<Mutex<CellGrid>>,
    fanout: WorkerFanout,
) {
    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;

    loop {
        match codec::recv_msg::<_, WorkerEvent>(&mut sock_rd).await {
            Ok(Some(ev)) => {
                handle_event(ev, &mirror, &fanout, &mut prev_cursor, &mut prev_modes);
            }
            Ok(None) => {
                debug!(pane_id = %fanout.pane_id, "worker closed its event stream");
                break;
            }
            Err(e) => {
                warn!(pane_id = %fanout.pane_id, "worker event read error: {e}");
                break;
            }
        }
    }

    if reap_child(child, &fanout.pane_id) {
        warn!(pane_id = %fanout.pane_id, "isolated VT worker crashed; surfacing fault (daemon and other sessions unaffected)");
        broadcast_fault(&fanout);
        // Ask the daemon to respawn the worker (the shell is still alive). If the
        // respawn channel is gone (shutdown), the pane simply stays faulted.
        let _ = fanout.fault_tx.send(fanout.pane_id.clone());
    }
}

/// Tell attached clients the pane's worker crashed. Uses the unbounded control
/// channel so the notice is never dropped (same channel `PaneEventSink` uses).
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
        WorkerEvent::Title { title } => fanout.event_sink.on_title(&title),
        WorkerEvent::Bell => fanout.event_sink.on_bell(),
        WorkerEvent::Osc52 {
            selection,
            base64_data,
        } => fanout.event_sink.on_osc52_copy(&selection, &base64_data),
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

/// Reap the worker (the daemon's direct child) so it does not linger as a
/// zombie, and report whether it died abnormally. A clean exit (code 0) is the
/// pane-close / handoff path; a signal death (SIGSEGV/SIGABRT) or non-zero exit
/// is a crash that should fault the pane. The worker has already exited by the
/// time we get here (we observed its socket EOF), so `wait` returns promptly.
fn reap_child(mut child: std::process::Child, pane_id: &str) -> bool {
    match child.wait() {
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

fn to_exit_status(status: ChildExitStatus) -> ExitStatus {
    match status {
        ChildExitStatus::Code(c) => ExitStatus::Code(c),
        ChildExitStatus::Signal(s) => ExitStatus::Signal(s),
        ChildExitStatus::Unknown => ExitStatus::Unknown,
    }
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
