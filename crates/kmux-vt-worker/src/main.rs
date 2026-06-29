//! Isolated per-pane VT worker (issue #126).
//!
//! kmuxd spawns one of these per pane when `session_isolation = "process"`. The
//! daemon owns the PTY but hands this worker a `dup` of the master fd over a
//! socketpair (`SCM_RIGHTS`); the worker runs the crash-prone libghostty-vt
//! pipeline — the *only* `unsafe`/FFI surface in the VT path — out-of-process.
//! A SIGSEGV here kills only this worker; the daemon (which retains the
//! authoritative master fd, so the shell survives) marks the pane faulted and
//! respawns, while every other session keeps running.
//!
//! ```text
//!  daemon ──Hello+fd──▶ worker: adopt PTY, build TermState ──Ready──▶ daemon
//!  daemon ──Input/Keys/Resize/…──▶ worker ──Diff/Cursor/Title/…──▶ daemon
//! ```
//!
//! The worker runs the SAME `kmux-vt-core` backend + diff engine kmuxd runs
//! in-process, so the diffs it emits are identical by construction; the daemon
//! stamps seqnos, mirrors scrollback, and fans out to clients exactly as it does
//! for an in-process pane.

use std::os::fd::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use nix::unistd::Pid;
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use kmux_pty::PtyProcess;
use kmux_pty::config::WindowSize;
use kmux_pty::process::ExitStatus;
use kmux_pty::session::{PtyReader, PtySession, PtyWriter};
use kmux_vt_core::backend::{
    BackendConfig, BackendEventSink, BackendSize, CapabilityHandles, ControlEvent,
};
use kmux_vt_core::diff_engine::DiffResult;
use kmux_vt_core::term_state::{TermState, new_term_state};
use kmux_worker_protocol::{
    ChildExitStatus, WORKER_PROTOCOL_VERSION, WorkerEvent, WorkerRequest, codec,
};

/// Env var carrying the worker end of the daemon↔worker socketpair, as a raw fd
/// number. The daemon dups the socket to this fd on the child and exports it.
const SOCKET_FD_ENV: &str = "KMUX_WORKER_SOCKET_FD";

fn main() -> anyhow::Result<()> {
    // Logs go to stderr; the daemon captures them with the worker's output. Keep
    // the env filter so `RUST_LOG=debug` works the same as kmuxd.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    // Route libghostty-vt's own diagnostics (unknown control sequences, …) into
    // the worker's stderr, which the daemon captures (issue #187). The default
    // `warn` filter already passes the `kmux::vt` target.
    kmux_vt_core::backend::install_vt_log_forwarding();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build worker runtime")?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let fd_num: RawFd = std::env::var(SOCKET_FD_ENV)
        .with_context(|| format!("{SOCKET_FD_ENV} not set"))?
        .parse()
        .with_context(|| format!("{SOCKET_FD_ENV} is not a valid fd number"))?;

    // SAFETY: the daemon dup'd the worker socketpair end to this fd and exported
    // its number; we own it now. Non-blocking so tokio can drive it.
    let stream = unsafe {
        let std_stream = std::os::unix::net::UnixStream::from_raw_fd(fd_num);
        std_stream
            .set_nonblocking(true)
            .context("set socket non-blocking")?;
        tokio::net::UnixStream::from_std(std_stream).context("adopt socket into tokio")?
    };

    // --- Handshake: receive Hello + the PTY master fd, adopt the PTY. ---
    let (hello, pty_fd) = codec::recv_with_fd::<WorkerRequest>(&stream)
        .await
        .context("recv Hello")?;
    let WorkerRequest::Hello {
        version,
        pane_id,
        pid,
        size,
        scrollback,
        kitty_graphics,
        kitty_keyboard,
    } = hello
    else {
        anyhow::bail!("first frame was not Hello");
    };
    if version != WORKER_PROTOCOL_VERSION {
        // Reply with our version so the daemon observes the mismatch, then bail;
        // the daemon falls back to running this pane in-process.
        let _ = codec::send_with_fd(
            &stream,
            &WorkerEvent::Ready {
                version: WORKER_PROTOCOL_VERSION,
            },
            None,
        )
        .await;
        anyhow::bail!(
            "worker protocol mismatch: daemon={version}, worker={WORKER_PROTOCOL_VERSION}"
        );
    }
    let pty_fd = pty_fd.context("Hello carried no PTY fd")?;

    let pty = PtyProcess::from_inherited(
        pty_fd,
        Pid::from_raw(pid),
        WindowSize {
            rows: size.rows,
            cols: size.cols,
        },
    )
    .context("adopt PTY fd")?;
    let session = PtySession::from_process(pty);
    // A worker exit or crash must NOT kill the shell — the daemon holds the
    // authoritative master fd and will respawn us. keep_alive suppresses the
    // SIGKILL in PtyProcess::drop; the leaked dup is moot since we are the
    // process dying.
    session.set_keep_alive(true).await;
    let (reader, writer) = session.clone().split().await.context("split PTY")?;

    // Capability atomics shared with the backend (updated by SetCapabilities).
    let kitty_graphics = Arc::new(AtomicBool::new(kitty_graphics));
    let kitty_keyboard = Arc::new(AtomicBool::new(kitty_keyboard));

    // Every WorkerEvent funnels through one channel; a single writer task
    // serialises socket writes (no concurrent writers).
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<WorkerEvent>();

    let term_state = Arc::new(Mutex::new(new_term_state(BackendConfig {
        size: BackendSize::from(size),
        capabilities: CapabilityHandles {
            kitty_graphics: kitty_graphics.clone(),
            kitty_keyboard: kitty_keyboard.clone(),
        },
        events: Arc::new(WorkerEventSink {
            events_tx: events_tx.clone(),
        }),
        scrollback: scrollback as usize,
    })));

    codec::send_with_fd(
        &stream,
        &WorkerEvent::Ready {
            version: WORKER_PROTOCOL_VERSION,
        },
        None,
    )
    .await
    .context("send Ready")?;
    debug!(pane_id, "worker ready");

    // --- Steady state: split the socket for concurrent I/O. ---
    let (mut sock_rd, mut sock_wr) = stream.into_split();

    let writer_task = tokio::spawn(async move {
        while let Some(ev) = events_rx.recv().await {
            if let Err(e) = codec::send_msg(&mut sock_wr, &ev).await {
                debug!("worker writer stopping: {e}");
                break;
            }
        }
    });

    let pty_task = {
        let term_state = term_state.clone();
        let events_tx = events_tx.clone();
        let session = session.clone();
        let pane_id = pane_id.clone();
        tokio::spawn(async move {
            pty_read_loop(reader, term_state, events_tx, session, &pane_id).await;
        })
    };

    // Daemon → worker request loop runs on the main task until Shutdown or the
    // daemon closes the socket.
    request_loop(
        &mut sock_rd,
        &term_state,
        &writer,
        &kitty_graphics,
        &kitty_keyboard,
        &events_tx,
    )
    .await;

    debug!(pane_id, "worker shutting down");
    pty_task.abort();
    writer_task.abort();
    Ok(())
}

/// Read PTY output, feed the emulator, and emit computed diffs — the worker's
/// half of `kmuxd`'s `session_diff_loop`. Also polls the foreground process name
/// so pane titles track command switches even without OSC 0/2.
async fn pty_read_loop(
    mut reader: PtyReader,
    term_state: Arc<Mutex<TermState>>,
    events_tx: mpsc::UnboundedSender<WorkerEvent>,
    session: PtySession,
    pane_id: &str,
) {
    let master_fd = reader.as_raw_fd();
    let mut buf = vec![0u8; 65536];
    let mut last_fg_name = String::new();

    let mut poll = tokio::time::interval(Duration::from_millis(500));
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    poll.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        {
                            // Hold the lock only for feed+coalesce; never across
                            // an await. Matches the in-process relay exactly.
                            let mut ts = term_state.lock().unwrap();
                            ts.feed(&buf[..n]);
                            loop {
                                match reader.try_read(&mut buf) {
                                    Ok(0) => break,
                                    Ok(m) => ts.feed(&buf[..m]),
                                    Err(_) => break, // WouldBlock or error
                                }
                            }
                        }
                        emit_diff(&term_state, &events_tx);
                    }
                    Err(e) => {
                        warn!(pane_id, "worker PTY read error: {e}");
                        break;
                    }
                }
            }
            _ = poll.tick() => {
                if let Some(name) = foreground_process_name(master_fd)
                    && name != last_fg_name
                {
                    last_fg_name = name.clone();
                    let _ = events_tx.send(WorkerEvent::Title { title: name });
                }
            }
        }
    }

    // PTY master returned EOF: the child exited. Report it so the daemon can
    // surface the runtime exit to clients (the sole exit signal for a foreign
    // child that cannot be `waitpid`-ed).
    let status = match tokio::time::timeout(Duration::from_secs(2), session.wait()).await {
        Ok(s) => s,
        Err(_) => ExitStatus::Unknown,
    };
    debug!(pane_id, ?status, "worker pane child exited");
    let _ = events_tx.send(WorkerEvent::ChildExit {
        status: to_wire_status(status),
    });
}

/// Compute one diff and emit it as a [`WorkerEvent`]. The daemon stamps the
/// seqno and fans out; this side stays seqno-agnostic so a worker restart never
/// regresses the sequence.
fn emit_diff(term_state: &Arc<Mutex<TermState>>, events_tx: &mpsc::UnboundedSender<WorkerEvent>) {
    let result = {
        let mut ts = term_state.lock().unwrap();
        ts.compute_diff()
    };
    match result {
        DiffResult::CellDiff {
            diff,
            scrollback_lines,
        } => {
            let _ = events_tx.send(WorkerEvent::Diff {
                diff,
                scrollback_lines,
            });
        }
        DiffResult::CursorOnly {
            cursor,
            modes,
            history_total,
        } => {
            let _ = events_tx.send(WorkerEvent::CursorOnly {
                cursor,
                modes,
                history_total,
            });
        }
        DiffResult::None => {}
    }
}

/// Handle daemon → worker requests until Shutdown or the socket closes.
async fn request_loop(
    sock_rd: &mut OwnedReadHalf,
    term_state: &Arc<Mutex<TermState>>,
    writer: &PtyWriter,
    kitty_graphics: &Arc<AtomicBool>,
    kitty_keyboard: &Arc<AtomicBool>,
    events_tx: &mpsc::UnboundedSender<WorkerEvent>,
) {
    loop {
        match codec::recv_msg::<_, WorkerRequest>(sock_rd).await {
            Ok(Some(req)) => {
                if handle_request(
                    req,
                    term_state,
                    writer,
                    kitty_graphics,
                    kitty_keyboard,
                    events_tx,
                )
                .await
                {
                    break; // Shutdown
                }
            }
            Ok(None) => break, // daemon closed the socket
            Err(e) => {
                warn!("worker request read error: {e}");
                break;
            }
        }
    }
}

/// Apply one request. Returns `true` on [`WorkerRequest::Shutdown`].
async fn handle_request(
    req: WorkerRequest,
    term_state: &Arc<Mutex<TermState>>,
    writer: &PtyWriter,
    kitty_graphics: &Arc<AtomicBool>,
    kitty_keyboard: &Arc<AtomicBool>,
    events_tx: &mpsc::UnboundedSender<WorkerEvent>,
) -> bool {
    match req {
        WorkerRequest::Hello { .. } => {
            warn!("worker: unexpected Hello after handshake; ignoring");
        }
        WorkerRequest::Input { data } => {
            if let Err(e) = writer.write_all(&data).await {
                warn!("worker: PTY write failed: {e}");
            }
        }
        WorkerRequest::Keys { events } => {
            // Encode under the lock so a mode-mutating sequence from an earlier
            // event is visible to later ones in the batch (matches io.rs).
            let bytes = {
                let ts = term_state.lock().unwrap();
                let mut b = Vec::with_capacity(events.len() * 32);
                for ev in &events {
                    b.extend_from_slice(&ts.encode_key_event(ev));
                }
                b
            };
            if !bytes.is_empty()
                && let Err(e) = writer.write_all(&bytes).await
            {
                warn!("worker: PTY key write failed: {e}");
            }
        }
        WorkerRequest::Paste { data } => {
            let bracketed = term_state.lock().unwrap().modes().bracketed_paste();
            let out = if bracketed {
                let mut buf = Vec::with_capacity(data.len() + 12);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(&data);
                buf.extend_from_slice(b"\x1b[201~");
                buf
            } else {
                data
            };
            if let Err(e) = writer.write_all(&out).await {
                warn!("worker: PTY paste write failed: {e}");
            }
        }
        WorkerRequest::Resize { size } => {
            // Resize only the emulator. The daemon owns the authoritative PTY
            // master fd and issues the kernel `TIOCSWINSZ` itself (via the
            // registry), so the worker must not also resize the shared PTY.
            let mut ts = term_state.lock().unwrap();
            ts.resize(BackendSize::from(size));
        }
        WorkerRequest::SnapshotRequest { req_id } => {
            let snapshot = {
                let ts = term_state.lock().unwrap();
                ts.snapshot()
            };
            let _ = events_tx.send(WorkerEvent::Snapshot { req_id, snapshot });
        }
        WorkerRequest::FetchHistory {
            req_id,
            start,
            count,
        } => {
            let (first_index, lines, history_total) = {
                let ts = term_state.lock().unwrap();
                let (first_index, lines) = ts.mirror_range(start, count);
                (first_index, lines, ts.history_total())
            };
            let _ = events_tx.send(WorkerEvent::History {
                req_id,
                first_index,
                lines,
                history_total,
            });
        }
        WorkerRequest::SetCapabilities {
            kitty_graphics: kg,
            kitty_keyboard: kk,
        } => {
            kitty_graphics.store(kg, Ordering::Relaxed);
            kitty_keyboard.store(kk, Ordering::Relaxed);
        }
        WorkerRequest::Shutdown => return true,
    }
    false
}

/// Bridges the `kmux-vt-core` backend event callbacks onto the worker's event
/// channel. Must not block (called from inside the VT parser); the unbounded
/// channel send satisfies that.
struct WorkerEventSink {
    events_tx: mpsc::UnboundedSender<WorkerEvent>,
}

impl BackendEventSink for WorkerEventSink {
    // The worker's single dispatch point for kmux's special VT sequences (issue
    // #187), mirroring the daemon's `PaneEventSink`. See
    // `kmux_vt_core::backend::ControlEvent` for the catalog.
    fn on_control_event(&self, event: ControlEvent<'_>) {
        match event {
            ControlEvent::Title(title) => {
                let _ = self.events_tx.send(WorkerEvent::Title {
                    title: title.to_string(),
                });
            }
            ControlEvent::Bell => {
                let _ = self.events_tx.send(WorkerEvent::Bell);
            }
            ControlEvent::Osc52Copy {
                selection,
                base64_data,
            } => {
                let _ = self.events_tx.send(WorkerEvent::Osc52 {
                    selection: selection.to_string(),
                    base64_data: base64_data.to_string(),
                });
            }
            // The worker protocol has no frame for progress / hyperlinks, so the
            // process-isolation path does not forward them (unchanged behaviour).
            ControlEvent::Progress { .. } | ControlEvent::Hyperlink { .. } => {}
        }
    }
}

fn to_wire_status(status: ExitStatus) -> ChildExitStatus {
    match status {
        ExitStatus::Code(c) => ChildExitStatus::Code(c),
        ExitStatus::Signal(s) => ChildExitStatus::Signal(s),
        ExitStatus::Unknown => ChildExitStatus::Unknown,
    }
}

/// Foreground process name on the PTY, via `tcgetpgrp` + `/proc/<pgid>/comm`.
/// Linux-only in effect (the `/proc` read returns `None` on macOS); copied from
/// `kmuxd`'s relay so the worker keeps fg-title tracking parity.
fn foreground_process_name(master_fd: RawFd) -> Option<String> {
    // SAFETY: master_fd is the PtyReader's dup'd PTY fd, valid for the loop's
    // lifetime; BorrowedFd is used only during this synchronous call.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(master_fd) };
    let pgid = nix::unistd::tcgetpgrp(fd).ok()?.as_raw();
    if pgid <= 0 {
        return None;
    }
    std::fs::read_to_string(format!("/proc/{pgid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
}
