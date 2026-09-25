//! The VT engine behind a pane: terminal emulation + PTY input.
//!
//! A pane's terminal half can run in one of two modes behind [`PaneEngine`]:
//!
//! - [`InProcessEngine`] — the emulator (`TermState`) and PTY writer live in the
//!   daemon. This is the default and today's behavior.
//! - `WorkerEngine` — the emulator runs in an isolated `kmux-vt-worker`
//!   subprocess so a libghostty-vt crash cannot take down the daemon (issue
//!   #126). Added in a later commit.
//!
//! `PaneRelay` holds a `PaneEngine` instead of touching `term_state`/`writer`
//! directly, so every VT read (snapshot, history) and PTY write (input, keys,
//! paste) routes through one seam that either variant can satisfy. Input is
//! queued ([`PaneEngine::enqueue_input`]) and written by a per-pane task, never
//! awaited by the caller (issue #206). The daemon's
//! seqno counter, scrollback `DiffBuffer`, and client fan-out stay on
//! `PaneRelay` and are shared by both variants.

mod in_process;
mod worker;

pub use in_process::InProcessEngine;
pub use worker::{WorkerEngine, WorkerFanout};

use kmux_protocol::messages::{GridSnapshot, KeyEvent, ScrollbackLine, TermSize};
use kmux_pty::error::{KmuxError, Result};
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;

/// Most client inputs a pane holds between arrival and its PTY write.
///
/// An input is one `PtyInput`, `PtyKeyBatch` or `PtyPaste` message, so this is
/// hundreds of keystroke batches: far more than a person types ahead of a
/// shell that is reading. It fills only when the child has stopped reading
/// stdin, and from then on further input is refused instead of piling up.
pub const INPUT_QUEUE_CAPACITY: usize = 256;

/// Why an input was not queued. The rejected input itself is dropped.
pub(super) type QueueRejected = TrySendError<()>;

/// Drop the payload a failed `try_send` hands back, keeping only why it failed.
pub(super) fn rejected<T>(e: &TrySendError<T>) -> QueueRejected {
    match e {
        TrySendError::Full(_) => TrySendError::Full(()),
        TrySendError::Closed(_) => TrySendError::Closed(()),
    }
}

/// One client input for a pane, queued in arrival order and turned into PTY
/// bytes by the pane's writer task.
#[derive(Debug)]
pub enum PaneInput {
    /// Raw bytes, written as they are.
    Bytes(Vec<u8>),
    /// Key events, encoded against the emulator's live modes when written, so
    /// a mode an earlier input switched is seen by a later one.
    Keys(Vec<KeyEvent>),
    /// Pasted text, wrapped in bracketed-paste markers when the emulator's
    /// live modes ask for them.
    Paste(Vec<u8>),
}

// Pane isolation is selected by `[daemon] session_isolation` in `kmuxd.toml`
// (overridable with `kmuxd --session-isolation`), resolved into
// `ServerApp::session_isolation` and read at pane creation. A worker spawn
// failure always falls back to the in-process engine.

/// The terminal emulator + PTY-input half of a pane.
pub enum PaneEngine {
    /// Emulator runs in the daemon (default).
    InProcess(InProcessEngine),
    /// Emulator runs in an isolated `kmux-vt-worker` subprocess (issue #126).
    Worker(WorkerEngine),
}

impl PaneEngine {
    /// Current full grid snapshot, for attach replay, resize re-seed, and
    /// checkpointing. Synchronous — callers hold the `sessions` lock. For a
    /// worker pane this reads the daemon-side mirror, never the worker.
    pub fn snapshot(&self) -> GridSnapshot {
        match self {
            Self::InProcess(e) => e.snapshot(),
            Self::Worker(e) => e.snapshot(),
        }
    }

    /// Resize the *emulator* to `size`. The kernel PTY is resized separately by
    /// the caller (it holds the master fd via the registry).
    pub fn resize_emulator(&self, size: TermSize) {
        match self {
            Self::InProcess(e) => e.resize_emulator(size),
            Self::Worker(e) => e.resize_emulator(size),
        }
    }

    /// Snapshot the grid and read up to `max_lines` of scrollback history, for a
    /// persistence checkpoint.
    pub fn checkpoint_grid(&self, max_lines: usize) -> (GridSnapshot, Vec<ScrollbackLine>) {
        match self {
            Self::InProcess(e) => e.checkpoint_grid(max_lines),
            Self::Worker(e) => e.checkpoint_grid(max_lines),
        }
    }

    /// Fetch a scrollback range as `(first_index, lines, history_total)`.
    pub async fn fetch_history(&self, start: u64, count: u32) -> (u64, Vec<ScrollbackLine>, u64) {
        match self {
            Self::InProcess(e) => e.mirror_range_and_total(start, count),
            Self::Worker(e) => e.mirror_range_and_total(start, count),
        }
    }

    /// Queue client input for the pane's writer task and return at once.
    ///
    /// The PTY write itself happens on that task, so a caller holding the
    /// `sessions` lock never waits on a child that stopped reading stdin. A
    /// full queue is refused with [`KmuxError::InputQueueFull`] rather than
    /// waited on; a writer that is gone (pane closing) is [`KmuxError::Closed`].
    pub fn enqueue_input(&self, pane_id: &str, input: PaneInput) -> Result<()> {
        let queued = match self {
            Self::InProcess(e) => e.enqueue_input(input),
            Self::Worker(e) => e.enqueue_input(input),
        };
        queued.map_err(|e| match e {
            TrySendError::Full(_) => KmuxError::InputQueueFull {
                pane_id: pane_id.to_string(),
            },
            TrySendError::Closed(_) => KmuxError::Closed,
        })
    }

    /// Push updated live kitty capability toggles to the emulator. In-process the
    /// backend reads shared atomics directly (no-op here); a worker is told over
    /// IPC.
    pub fn set_capabilities(&self, kitty_graphics: bool, kitty_keyboard: bool) {
        match self {
            Self::InProcess(_) => {}
            Self::Worker(e) => e.set_capabilities(kitty_graphics, kitty_keyboard),
        }
    }

    /// Whether this pane runs in an isolated worker subprocess.
    pub fn is_worker(&self) -> bool {
        matches!(self, Self::Worker(_))
    }

    /// OS pid of the isolated worker subprocess, or `None` for an in-process pane.
    pub fn worker_pid(&self) -> Option<u32> {
        match self {
            Self::InProcess(_) => None,
            Self::Worker(e) => Some(e.child_pid()),
        }
    }

    /// Abort the pane's relay task and swap in a no-op handle, returning the real
    /// one so the caller can await its cancellation (used by handoff quiesce).
    pub fn abort_relay_task(&mut self) -> JoinHandle<()> {
        match self {
            Self::InProcess(e) => e.abort_relay_task(),
            Self::Worker(e) => e.abort_relay_task(),
        }
    }
}
