//! Daemon ↔ worker IPC contract for kmux session **process isolation** (issue #126).
//!
//! The daemon hosts a session's PTY but runs the crash-prone VT pipeline
//! (libghostty-vt FFI + diff engine) inside a separate `kmux-vt-worker`
//! subprocess, so a SIGSEGV in the emulator faults one pane instead of taking
//! down the daemon and every other session. This crate defines the wire
//! contract between the two:
//!
//! - [`WorkerRequest`] — daemon → worker (input, keys, resize, snapshot/history
//!   requests, capability toggles, shutdown).
//! - [`WorkerEvent`] — worker → daemon (computed diffs, cursor-only updates,
//!   title/bell/OSC 52 events, snapshot/history responses, child-exit, fault).
//!
//! # Transport
//!
//! One `AF_UNIX`/`SOCK_STREAM` socketpair. The very first frame is the daemon's
//! [`WorkerRequest::Hello`], which carries the PTY master fd as `SCM_RIGHTS`
//! ancillary data — the *only* fd that ever crosses the link. The worker adopts
//! it and replies [`WorkerEvent::Ready`]; that fd-carrying handshake is
//! lock-step (see [`codec::send_with_fd`] / [`codec::recv_with_fd`]). After the
//! handshake both ends split the stream and exchange fd-less, length-prefixed
//! postcard frames concurrently ([`codec::send_msg`] / [`codec::recv_msg`]).
//!
//! # Versioning
//!
//! Like every cross-component boundary in kmux, the link is versioned by
//! [`WORKER_PROTOCOL_VERSION`]. The daemon and worker are always the same build
//! in normal operation (the daemon execs the worker binary next to itself), but
//! a stale binary after an in-place upgrade is possible; on a `Hello`/`Ready`
//! version mismatch the daemon kills the worker and falls back to running the
//! pane in-process, which is always safe.

pub mod codec;

use serde::{Deserialize, Serialize};

use kmux_protocol::messages::{
    CursorState, GridSnapshot, KeyEvent, ScrollbackLine, TermModes, TermSize, TerminalDiff,
};

/// Version of the daemon ↔ worker IPC contract.
///
/// Bump on ANY change to [`WorkerRequest`], [`WorkerEvent`], or their payloads'
/// wire layout. Postcard is not self-describing, so a field add/remove or
/// variant reorder is a silent wire break — the version check is the guard.
pub const WORKER_PROTOCOL_VERSION: u32 = 1;

/// Daemon → worker control frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerRequest {
    /// Opening frame. Carries the PTY master fd as `SCM_RIGHTS` ancillary data
    /// (the only fd on the link). The worker adopts the fd as a foreign child —
    /// the daemon retains the authoritative master fd, so the shell survives a
    /// worker crash and can be re-adopted by a respawned worker.
    Hello {
        /// Must equal [`WORKER_PROTOCOL_VERSION`]; mismatch → daemon falls back
        /// to the in-process engine for this pane.
        version: u32,
        /// `{word_id}/{pane_index}` registry key, for logging/correlation.
        pane_id: String,
        /// Child PID behind the adopted fd (foreign-child liveness polling).
        pid: i32,
        /// Initial emulator + PTY dimensions.
        size: TermSize,
        /// Maximum scrollback lines the emulator retains.
        scrollback: u32,
        /// Live kitty-graphics capability (intersected across attached clients).
        kitty_graphics: bool,
        /// Live kitty-keyboard capability (intersected across attached clients).
        kitty_keyboard: bool,
    },
    /// Raw client bytes to write to the PTY (fire-and-forget).
    Input { data: Vec<u8> },
    /// Structured key events; the worker encodes each against the emulator's
    /// live mode state (DECCKM, kitty flags, modifyOtherKeys) and writes the
    /// bytes to the PTY. Fire-and-forget — the encoder needs the live state the
    /// worker owns, so it cannot stay daemon-side.
    Keys { events: Vec<KeyEvent> },
    /// Paste payload; the worker wraps it in bracketed-paste markers when the
    /// emulator's live `modes()` has bracketed paste enabled, then writes it.
    Paste { data: Vec<u8> },
    /// Resize the emulator and issue `TIOCSWINSZ` on the PTY (the worker holds
    /// the fd). No reply; a forced snapshot follows from the daemon side.
    Resize { size: TermSize },
    /// Request a full grid snapshot (reply: [`WorkerEvent::Snapshot`] with the
    /// same `req_id`). Off the hot path — used on attach/resize.
    SnapshotRequest { req_id: u64 },
    /// Request scrollback history lines (reply: [`WorkerEvent::History`]).
    FetchHistory { req_id: u64, start: u64, count: u32 },
    /// Update the live kitty capability toggles after a client attach/detach.
    SetCapabilities {
        kitty_graphics: bool,
        kitty_keyboard: bool,
    },
    /// Graceful shutdown: drain and exit 0. The shell survives because the
    /// daemon holds the authoritative master fd.
    Shutdown,
}

/// Worker → daemon event frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WorkerEvent {
    /// Handshake ack; echoes the negotiated [`WORKER_PROTOCOL_VERSION`]. A
    /// mismatch (or no `Ready` before the worker dies) makes the daemon fall
    /// back to the in-process engine.
    Ready { version: u32 },
    /// A computed cell diff. **Unsequenced** — the daemon stamps the monotonic
    /// `SequenceNo` so it survives a worker restart. `scrollback_lines` are the
    /// lines appended to the mirror this frame (oldest first), emitted by the
    /// daemon as a separate `ScrollbackAppend`, exactly as the in-process relay
    /// does.
    Diff {
        diff: TerminalDiff,
        scrollback_lines: Vec<ScrollbackLine>,
    },
    /// No cells changed, but the cursor position or terminal modes did.
    CursorOnly {
        cursor: CursorState,
        modes: TermModes,
        history_total: u64,
    },
    /// OSC 0/2 window title (`BackendEventSink::on_title`).
    Title { title: String },
    /// BEL (`BackendEventSink::on_bell`).
    Bell,
    /// OSC 52 clipboard write (`BackendEventSink::on_osc52_copy`): normalized
    /// selection target and still-base64 payload.
    Osc52 {
        selection: String,
        base64_data: String,
    },
    /// Reply to [`WorkerRequest::SnapshotRequest`].
    Snapshot { req_id: u64, snapshot: GridSnapshot },
    /// Reply to [`WorkerRequest::FetchHistory`].
    History {
        req_id: u64,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
        history_total: u64,
    },
    /// The PTY child exited (master returned EOF). The daemon surfaces this to
    /// clients as the pane's runtime exit.
    ChildExit { status: ChildExitStatus },
    /// Best-effort note emitted just before the worker dies from a recoverable
    /// Rust panic in the safe glue (a libghostty-vt memory fault gives no such
    /// warning — the daemon detects that purely from the worker's signal death).
    Fault { detail: String },
}

/// How the PTY child terminated, mirrored from `kmux_pty::ExitStatus` so this
/// crate stays free of a `kmux-pty` dependency. The daemon maps it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChildExitStatus {
    /// Exited normally with this code.
    Code(i32),
    /// Killed by this signal.
    Signal(i32),
    /// State could not be determined.
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::messages::{CellState, CursorState, DiffOp, TermModes};

    /// Postcard is not self-describing, so every variant must survive a
    /// round-trip unchanged or the daemon and worker silently desync. Compare by
    /// re-encoding (the payload types are not all `PartialEq`).
    fn rt_request(msg: &WorkerRequest) {
        let bytes = postcard::to_allocvec(msg).expect("encode");
        let back: WorkerRequest = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(
            postcard::to_allocvec(&back).expect("re-encode"),
            bytes,
            "request did not round-trip: {msg:?}"
        );
    }

    fn rt_event(msg: &WorkerEvent) {
        let bytes = postcard::to_allocvec(msg).expect("encode");
        let back: WorkerEvent = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(
            postcard::to_allocvec(&back).expect("re-encode"),
            bytes,
            "event did not round-trip: {msg:?}"
        );
    }

    #[test]
    fn all_requests_round_trip() {
        rt_request(&WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION,
            pane_id: "eagle/0".into(),
            pid: 4242,
            size: TermSize::default(),
            scrollback: 50_000,
            kitty_graphics: true,
            kitty_keyboard: false,
        });
        rt_request(&WorkerRequest::Input {
            data: b"ls -la\n".to_vec(),
        });
        rt_request(&WorkerRequest::Keys { events: vec![] });
        rt_request(&WorkerRequest::Paste {
            data: b"pasted".to_vec(),
        });
        rt_request(&WorkerRequest::Resize {
            size: TermSize {
                rows: 40,
                cols: 132,
                pixel_width: 0,
                pixel_height: 0,
            },
        });
        rt_request(&WorkerRequest::SnapshotRequest { req_id: 7 });
        rt_request(&WorkerRequest::FetchHistory {
            req_id: 8,
            start: 100,
            count: 50,
        });
        rt_request(&WorkerRequest::SetCapabilities {
            kitty_graphics: false,
            kitty_keyboard: true,
        });
        rt_request(&WorkerRequest::Shutdown);
    }

    #[test]
    fn all_events_round_trip() {
        rt_event(&WorkerEvent::Ready {
            version: WORKER_PROTOCOL_VERSION,
        });
        rt_event(&WorkerEvent::Diff {
            diff: TerminalDiff {
                ops: vec![DiffOp::Cell {
                    row: 0,
                    col: 5,
                    cell: CellState::default(),
                }],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                history_total: 3,
                scrollback_reset: None,
            },
            scrollback_lines: vec![vec![CellState::default(); 4].into()],
        });
        rt_event(&WorkerEvent::CursorOnly {
            cursor: CursorState::default(),
            modes: TermModes::EMPTY,
            history_total: 9,
        });
        rt_event(&WorkerEvent::Title {
            title: "vim — main.rs".into(),
        });
        rt_event(&WorkerEvent::Bell);
        rt_event(&WorkerEvent::Osc52 {
            selection: "c".into(),
            base64_data: "aGk=".into(),
        });
        rt_event(&WorkerEvent::Snapshot {
            req_id: 1,
            snapshot: GridSnapshot {
                rows: 1,
                cols: 1,
                cells: vec![CellState::default()],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                history_total: 0,
                scrollback_base: 0,
                scrollback_tail: vec![],
            },
        });
        rt_event(&WorkerEvent::History {
            req_id: 2,
            first_index: 10,
            lines: vec![vec![CellState::default(); 2].into()],
            history_total: 12,
        });
        for status in [
            ChildExitStatus::Code(0),
            ChildExitStatus::Signal(11),
            ChildExitStatus::Unknown,
        ] {
            rt_event(&WorkerEvent::ChildExit { status });
        }
        rt_event(&WorkerEvent::Fault {
            detail: "poisoned term_state mutex".into(),
        });
    }

    #[test]
    fn hello_version_is_observable_for_mismatch() {
        // A worker built against a different contract sends a version the daemon
        // can read off `Hello` before trusting any other field — the hook the
        // daemon uses to refuse and fall back to in-process.
        let bytes = postcard::to_allocvec(&WorkerRequest::Hello {
            version: WORKER_PROTOCOL_VERSION + 1,
            pane_id: "eagle/0".into(),
            pid: 1,
            size: TermSize::default(),
            scrollback: 0,
            kitty_graphics: false,
            kitty_keyboard: false,
        })
        .expect("encode");
        match postcard::from_bytes::<WorkerRequest>(&bytes).expect("decode") {
            WorkerRequest::Hello { version, .. } => {
                assert_ne!(version, WORKER_PROTOCOL_VERSION)
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }
}
