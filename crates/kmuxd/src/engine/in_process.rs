//! In-process VT engine: the emulator and PTY writer live in the daemon.
//!
//! This is the default path and preserves the daemon's original behavior
//! exactly — every method is the same `term_state.lock()` / `writer.write_all`
//! operation the call sites used before the [`PaneEngine`](super::PaneEngine)
//! seam was introduced.

use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{GridSnapshot, KeyEvent, ScrollbackLine, TermSize};
use kmux_pty::error::Result;
use kmux_pty::session::PtyWriter;
use tokio::task::JoinHandle;

use crate::backend::BackendSize;
use crate::term_state::TermState;

/// VT emulator + PTY writer running inside the daemon.
pub struct InProcessEngine {
    /// Server-side VT emulation state for this pane.
    term_state: Arc<Mutex<TermState>>,
    /// Write half of the pane's PTY, for forwarding client input.
    writer: PtyWriter,
    /// Background relay task (`session_diff_loop`) reading the PTY.
    task: JoinHandle<()>,
}

impl InProcessEngine {
    pub fn new(term_state: Arc<Mutex<TermState>>, writer: PtyWriter, task: JoinHandle<()>) -> Self {
        Self {
            term_state,
            writer,
            task,
        }
    }

    pub(super) fn snapshot(&self) -> GridSnapshot {
        self.term_state.lock().unwrap().snapshot()
    }

    pub(super) fn resize_emulator(&self, size: TermSize) {
        self.term_state
            .lock()
            .unwrap()
            .resize(BackendSize::from(size));
    }

    pub(super) fn checkpoint_grid(&self, max_lines: usize) -> (GridSnapshot, Vec<ScrollbackLine>) {
        let ts = self.term_state.lock().unwrap();
        let grid = ts.snapshot();
        let size = ts.history_size();
        let start = size.saturating_sub(max_lines);
        let count = size - start;
        let lines = if count > 0 {
            ts.read_history_lines(start, count)
        } else {
            vec![]
        };
        (grid, lines)
    }

    pub(super) fn mirror_range_and_total(
        &self,
        start: u64,
        count: u32,
    ) -> (u64, Vec<ScrollbackLine>, u64) {
        let ts = self.term_state.lock().unwrap();
        let (first_index, lines) = ts.mirror_range(start, count);
        (first_index, lines, ts.history_total())
    }

    pub(super) async fn write_input(&self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data).await
    }

    pub(super) async fn write_keys(&self, events: &[KeyEvent]) -> Result<()> {
        // Encode under the lock so a mode-mutating sequence from an earlier event
        // is visible to later ones in the batch.
        let bytes = {
            let ts = self.term_state.lock().unwrap();
            let mut bytes = Vec::with_capacity(events.len() * 32);
            for ev in events {
                bytes.extend_from_slice(&ts.encode_key_event(ev));
            }
            bytes
        };
        if bytes.is_empty() {
            return Ok(());
        }
        self.writer.write_all(&bytes).await
    }

    pub(super) async fn write_paste(&self, data: &[u8]) -> Result<()> {
        let bracketed = self.term_state.lock().unwrap().modes().bracketed_paste();
        if bracketed {
            let mut buf = Vec::with_capacity(data.len() + 12);
            buf.extend_from_slice(b"\x1b[200~");
            buf.extend_from_slice(data);
            buf.extend_from_slice(b"\x1b[201~");
            self.writer.write_all(&buf).await
        } else {
            self.writer.write_all(data).await
        }
    }

    pub(super) fn abort_relay_task(&mut self) -> JoinHandle<()> {
        self.task.abort();
        std::mem::replace(&mut self.task, tokio::spawn(async {}))
    }
}
