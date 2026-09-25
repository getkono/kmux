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
        lock_term_state(&self.term_state).resize(BackendSize::from(size));
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
        self.input_tx.try_send(input).map_err(|e| rejected(&e))
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kmux_protocol::messages::{KeyAction, KeyCode, KeyEvent, KeyMods};

    use super::*;
    use crate::fixtures::{fixture_pty, fixture_term_state};

    /// A bound on waits for a real PTY.
    const WAIT: Duration = Duration::from_secs(10);

    fn key_a() -> KeyEvent {
        KeyEvent {
            code: KeyCode::A,
            mods: KeyMods::empty(),
            action: KeyAction::Press,
            text: "a".to_string(),
            unshifted_codepoint: u32::from('a'),
        }
    }

    /// History reads go to the emulator's scrollback mirror: the first
    /// index served, the lines, and the total ever scrolled off.
    #[tokio::test]
    async fn mirror_range_and_total_reads_the_scrollback_mirror() {
        let engine = InProcessEngine {
            term_state: fixture_term_state(2, 10),
            input_tx: mpsc::channel(1).0,
            input_task: tokio::spawn(async {}),
            task: tokio::spawn(async {}),
            response_task: tokio::spawn(async {}),
        };
        {
            let mut ts = engine.term_state.lock().unwrap();
            ts.feed(b"one\r\ntwo\r\nthree\r\nfour");
            // The mirror is filled as diffs are computed, as the relay does.
            let _ = ts.compute_diff();
        }
        let (first, lines, total) = engine.mirror_range_and_total(1, 5);
        assert_eq!((first, lines.len(), total), (1, 1, 2));
    }

    /// Bytes pass through, keys are encoded, and a paste is bracketed only
    /// once the program has switched bracketed paste on.
    #[test]
    fn input_bytes_encodes_each_input_kind_against_the_live_modes() {
        let ts = fixture_term_state(4, 20);
        assert_eq!(input_bytes(&ts, PaneInput::Bytes(b"raw".to_vec())), b"raw");
        assert_eq!(input_bytes(&ts, PaneInput::Keys(vec![key_a()])), b"a");
        assert_eq!(input_bytes(&ts, PaneInput::Paste(b"p".to_vec())), b"p");

        ts.lock().unwrap().feed(b"\x1b[?2004h");
        assert_eq!(
            input_bytes(&ts, PaneInput::Paste(b"p".to_vec())),
            b"\x1b[200~p\x1b[201~"
        );
    }

    fn engine_on(writer: PtyWriter) -> InProcessEngine {
        InProcessEngine::new(
            "eagle/0".to_string(),
            fixture_term_state(4, 20),
            writer,
            tokio::spawn(async {}),
            mpsc::unbounded_channel().1,
        )
    }

    /// Queued input reaches the pane's program: `cat` echoes it back. A real
    /// PTY because the writer's only output is the PTY (R7).
    #[tokio::test]
    async fn queued_input_is_written_to_the_pty() {
        let (_session, mut reader, writer) = fixture_pty("cat").await;
        let engine = engine_on(writer);
        engine
            .enqueue_input(PaneInput::Bytes(b"ping\n".to_vec()))
            .expect("room in the queue");

        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        tokio::time::timeout(WAIT, async {
            while !seen.windows(4).any(|w| w == b"ping") {
                let n = reader.read(&mut buf).await.unwrap();
                seen.extend_from_slice(&buf[..n]);
            }
        })
        .await
        .expect("the input reaches the program");
    }

    /// Dropping the engine stops its input writer outright, rather than
    /// leaving it to end once its queue closes: a writer blocked on a program
    /// that never reads would otherwise hold the PTY past the pane's close.
    /// A sender kept open stands in for that busy writer, so only the abort
    /// can end the task.
    #[tokio::test]
    async fn dropping_the_engine_stops_its_input_writer() {
        let engine = engine_on(PtyWriter::sink().unwrap());
        let _still_open = engine.input_tx.clone();
        let input_task = engine.input_task.abort_handle();

        drop(engine);
        tokio::time::timeout(WAIT, async {
            while !input_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the input writer ends with the engine");
    }
}
