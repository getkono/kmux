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
//! paste) routes through one seam that either variant can satisfy. The daemon's
//! seqno counter, scrollback `DiffBuffer`, and client fan-out stay on
//! `PaneRelay` and are shared by both variants.

mod in_process;

pub use in_process::InProcessEngine;

use kmux_protocol::messages::{CellState, GridSnapshot, KeyEvent, TermSize};
use kmux_pty::error::Result;
use tokio::task::JoinHandle;

/// The terminal emulator + PTY-input half of a pane.
pub enum PaneEngine {
    /// Emulator runs in the daemon (default).
    InProcess(InProcessEngine),
}

impl PaneEngine {
    /// Current full grid snapshot, for attach replay, resize re-seed, and
    /// checkpointing. Synchronous — callers hold the `sessions` lock.
    pub fn snapshot(&self) -> GridSnapshot {
        match self {
            Self::InProcess(e) => e.snapshot(),
        }
    }

    /// Resize the *emulator* to `size`. The kernel PTY is resized separately by
    /// the caller (it holds the master fd via the registry).
    pub fn resize_emulator(&self, size: TermSize) {
        match self {
            Self::InProcess(e) => e.resize_emulator(size),
        }
    }

    /// Snapshot the grid and read up to `max_lines` of scrollback history, for a
    /// persistence checkpoint.
    pub fn checkpoint_grid(&self, max_lines: usize) -> (GridSnapshot, Vec<Vec<CellState>>) {
        match self {
            Self::InProcess(e) => e.checkpoint_grid(max_lines),
        }
    }

    /// Fetch a scrollback range as `(first_index, lines, history_total)`.
    pub async fn fetch_history(&self, start: u64, count: u32) -> (u64, Vec<Vec<CellState>>, u64) {
        match self {
            Self::InProcess(e) => e.mirror_range_and_total(start, count),
        }
    }

    /// Forward raw client bytes to the PTY.
    pub async fn write_input(&self, data: &[u8]) -> Result<()> {
        match self {
            Self::InProcess(e) => e.write_input(data).await,
        }
    }

    /// Encode `events` against the emulator's live mode state and write the
    /// bytes to the PTY (encoding stays with the emulator).
    pub async fn write_keys(&self, events: &[KeyEvent]) -> Result<()> {
        match self {
            Self::InProcess(e) => e.write_keys(events).await,
        }
    }

    /// Write a paste payload, wrapping it in bracketed-paste markers when the
    /// emulator's live modes request it.
    pub async fn write_paste(&self, data: &[u8]) -> Result<()> {
        match self {
            Self::InProcess(e) => e.write_paste(data).await,
        }
    }

    /// Abort the pane's relay task and swap in a no-op handle, returning the real
    /// one so the caller can await its cancellation (used by handoff quiesce).
    pub fn abort_relay_task(&mut self) -> JoinHandle<()> {
        match self {
            Self::InProcess(e) => e.abort_relay_task(),
        }
    }
}
