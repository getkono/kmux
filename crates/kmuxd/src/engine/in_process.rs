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
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::BackendSize;
use crate::term_state::TermState;

/// VT emulator + PTY writer running inside the daemon.
pub struct InProcessEngine {
    /// Server-side VT emulation state for this pane.
    term_state: Arc<Mutex<TermState>>,
    /// Write half of the pane's PTY, for forwarding client input. Shared
    /// (`Arc`) with the terminal-query-reply drain task; `PtyWriter::write_all`
    /// serialises both through its interior mutex.
    writer: Arc<PtyWriter>,
    /// Background relay task (`session_diff_loop`) reading the PTY.
    task: JoinHandle<()>,
    /// Drains terminal query replies (DSR/DA/…) queued by the pane's event sink
    /// and writes them to `writer`. Aborted on drop.
    response_task: JoinHandle<()>,
}

impl InProcessEngine {
    /// Build the engine and spawn the terminal-query-reply drain.
    ///
    /// `response_rx` is the receiving half of the channel the pane's
    /// [`PaneEventSink`](crate::app::PaneEventSink) pushes reply bytes onto (via
    /// `set_pty_response_sender`). The drain writes them back to the child,
    /// serialised with user input through the shared `writer`.
    pub fn new(
        pane_id: String,
        term_state: Arc<Mutex<TermState>>,
        writer: PtyWriter,
        task: JoinHandle<()>,
        response_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) -> Self {
        let writer = Arc::new(writer);
        let response_task = tokio::spawn(pty_response_writer(
            pane_id,
            response_rx,
            Arc::clone(&writer),
        ));
        Self {
            term_state,
            writer,
            task,
            response_task,
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

impl Drop for InProcessEngine {
    fn drop(&mut self) {
        // The relay task's lifecycle is managed explicitly (`abort_relay_task`
        // / handoff quiesce); the response drain has no such handshake, so abort
        // it here. It would also end on its own once the sink's sender drops.
        self.response_task.abort();
    }
}

/// Drain terminal query replies (DSR/DA/DECRQM/…) queued by the pane's event
/// sink and write each back to the child, in FIFO order, until the channel
/// closes (pane teardown). The writes share `writer` with user input, so the
/// `PtyWriter`'s interior mutex serialises them — a reply can never interleave
/// within a concurrent keystroke's bytes. A write error (a closed PTY on a
/// dying pane) is logged and skipped so shutdown never blocks.
async fn pty_response_writer(
    pane_id: String,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    writer: Arc<PtyWriter>,
) {
    while let Some(bytes) = rx.recv().await {
        if let Err(e) = writer.write_all(&bytes).await {
            tracing::debug!(pane_id, error = %e, "pty query-reply write failed (pane closing?)");
        }
    }
}
