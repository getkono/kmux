//! In-process VT engine: the emulator and PTY writer live in the daemon.
//!
//! This is the default path. Reads are the same `term_state` lock the call
//! sites used before the [`PaneEngine`](super::PaneEngine) seam was introduced;
//! client input goes through a bounded queue drained by the pane's input
//! writer task (issue #206).

use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{GridSnapshot, ScrollbackLine, TermSize};
use kmux_pty::session::PtyWriter;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{INPUT_QUEUE_CAPACITY, PaneInput, QueueRejected, rejected};
use crate::backend::BackendSize;
use crate::lock::lock_term_state;
use crate::term_state::TermState;

/// VT emulator + PTY writer running inside the daemon.
pub struct InProcessEngine {
    /// Server-side VT emulation state for this pane.
    term_state: Arc<Mutex<TermState>>,
    /// Client input waiting for [`pty_input_writer`]. Bounded: a pane whose
    /// child stopped reading refuses input instead of growing without limit.
    input_tx: mpsc::Sender<PaneInput>,
    /// Drains `input_tx` to the PTY. Aborted on drop.
    input_task: JoinHandle<()>,
    /// Background relay task (`session_diff_loop`) reading the PTY.
    task: JoinHandle<()>,
    /// Drains terminal query replies (DSR/DA/…) queued by the pane's event sink
    /// and writes them to `writer`. Aborted on drop.
    response_task: JoinHandle<()>,
}

impl InProcessEngine {
    /// Build the engine and spawn the input writer and the
    /// terminal-query-reply drain.
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
        let (input_tx, input_rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let input_task = tokio::spawn(pty_input_writer(
            pane_id.clone(),
            input_rx,
            Arc::clone(&term_state),
            Arc::clone(&writer),
        ));
        let response_task = tokio::spawn(pty_response_writer(pane_id, response_rx, writer));
        Self {
            term_state,
            input_tx,
            input_task,
            task,
            response_task,
        }
    }

    pub(super) fn snapshot(&self) -> GridSnapshot {
        lock_term_state(&self.term_state).snapshot()
    }

    pub(super) fn resize_emulator(&self, size: TermSize) {
        self.term_state
            .lock()
            .unwrap()
            .resize(BackendSize::from(size));
    }

    pub(super) fn checkpoint_grid(&self, max_lines: usize) -> (GridSnapshot, Vec<ScrollbackLine>) {
        let ts = lock_term_state(&self.term_state);
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
        let ts = lock_term_state(&self.term_state);
        let (first_index, lines) = ts.mirror_range(start, count);
        (first_index, lines, ts.history_total())
    }

    pub(super) fn enqueue_input(&self, input: PaneInput) -> Result<(), QueueRejected> {
        self.input_tx.try_send(input).map_err(rejected)
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
        // The input writer may be blocked on a child that never reads, so it
        // is aborted too rather than left holding the PTY.
        self.input_task.abort();
        self.response_task.abort();
    }
}

/// Write a pane's queued client input to its PTY, in arrival order, until the
/// queue closes (pane teardown). This is the only place client input awaits
/// the PTY, and it holds no daemon lock while it does: a child that stops
/// reading blocks this task and fills this pane's queue, nothing else.
///
/// A write error (a closed PTY on a dying pane) is logged and skipped.
async fn pty_input_writer(
    pane_id: String,
    mut rx: mpsc::Receiver<PaneInput>,
    term_state: Arc<Mutex<TermState>>,
    writer: Arc<PtyWriter>,
) {
    while let Some(input) = rx.recv().await {
        let bytes = input_bytes(&term_state, input);
        if bytes.is_empty() {
            continue;
        }
        if let Err(e) = writer.write_all(&bytes).await {
            tracing::debug!(pane_id, error = %e, "pty input write failed (pane closing?)");
        }
    }
}

/// The PTY bytes for one input. Keys and pastes read the emulator's live
/// modes here, at write time, so they see every mode switch the output before
/// them produced.
fn input_bytes(term_state: &Mutex<TermState>, input: PaneInput) -> Vec<u8> {
    match input {
        PaneInput::Bytes(bytes) => bytes,
        PaneInput::Keys(events) => {
            // Encode under one lock hold so a mode-mutating sequence from an
            // earlier event is visible to later ones in the batch.
            let ts = lock_term_state(term_state);
            let mut bytes = Vec::with_capacity(events.len() * 32);
            for ev in &events {
                bytes.extend_from_slice(&ts.encode_key_event(ev));
            }
            bytes
        }
        PaneInput::Paste(data) => {
            if lock_term_state(term_state).modes().bracketed_paste() {
                let mut buf = Vec::with_capacity(data.len() + 12);
                buf.extend_from_slice(b"\x1b[200~");
                buf.extend_from_slice(&data);
                buf.extend_from_slice(b"\x1b[201~");
                buf
            } else {
                data
            }
        }
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
