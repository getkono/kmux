//! Off-UI-thread grid apply worker (issue #182, §1).
//!
//! The authoritative [`GridContent`] for each pane lives on a dedicated worker
//! thread owned by the `SessionManager`. The UI thread enqueues content
//! mutations as [`ApplyJob`]s; the worker applies them in order and publishes a
//! fresh immutable `Arc<GridContent>` per touched pane via an [`ArcSwap`]
//! double-buffer. Renderers load the published `Arc` — an atomic acquire load
//! that always yields a fully-constructed prior value, so a reader can never
//! observe a torn `(generation, grid)` pair. The grid-digest oracle is
//! content-blind to such tears, so this handoff carries its own invariant: a
//! reader/writer property test (`tests/grid_apply_worker.rs`) drives a real
//! worker against a concurrent reader and asserts no torn `(dimensions, cells,
//! generation)` tuple is ever observed, plus that a `Published` grid matches a
//! synchronous `Local` reference. (A `loom` model would be the textbook tool
//! here, but loom disables `tokio`'s I/O under `--cfg loom`, and this crate's
//! transport stack needs it; the real-threads property test stands in.)
//!
//! View-state consequences of an apply (snap-to-bottom, selection
//! reconciliation) travel back as [`WorkerNote`]s for the UI thread to apply to
//! the pane's `GridView`, which it owns exclusively.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

use arc_swap::ArcSwap;
use kmux_protocol::messages::{
    CursorState, GridSnapshot, PaneId, ScrollbackLine, SequenceNo, TermModes, TerminalDiff,
};
use tracing::warn;

use super::content::{ApplyEffect, GridContent};

/// Shared, atomically-swappable publication slot for one pane's content.
pub type Published = Arc<ArcSwap<GridContent>>;

/// A content mutation handed to the apply worker. `order` is a per-pane
/// monotonic counter assigned by the publishing facade so the worker can assert
/// jobs arrive without reorder or loss (the mpsc channel guarantees this; the
/// assertion is a cheap bug guard).
pub enum ApplyJob {
    Register {
        pane_id: PaneId,
        published: Published,
        rows: usize,
        cols: usize,
    },
    Snapshot {
        pane_id: PaneId,
        order: u64,
        snapshot: Box<GridSnapshot>,
    },
    Diff {
        pane_id: PaneId,
        order: u64,
        diff: Box<TerminalDiff>,
    },
    Cursor {
        pane_id: PaneId,
        order: u64,
        cursor: CursorState,
        modes: TermModes,
    },
    ScrollbackAppend {
        pane_id: PaneId,
        order: u64,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
    },
    HistoryLines {
        pane_id: PaneId,
        order: u64,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
        history_total: u64,
    },
    Clear {
        pane_id: PaneId,
        order: u64,
    },
    Resize {
        pane_id: PaneId,
        order: u64,
        rows: u16,
        cols: u16,
    },
    /// Verify the pane's content digest against the daemon's certified `hash`.
    Digest {
        pane_id: PaneId,
        seqno: SequenceNo,
        hash: u128,
    },
    Forget {
        pane_id: PaneId,
    },
    /// Drop all pane state (reconnect).
    Reset,
    /// Publish everything pending, then acknowledge — lets a test observe the
    /// effect of previously-enqueued jobs synchronously.
    Barrier(SyncSender<()>),
    Shutdown,
}

/// A consequence the worker reports back to the UI thread.
pub enum WorkerNote {
    /// Apply this view-state effect to the pane's `GridView`, in order.
    Effect {
        pane_id: PaneId,
        effect: ApplyEffect,
    },
    /// The pane's content digest diverged from the daemon's at `seqno`; resync.
    DigestMismatch { pane_id: PaneId, seqno: SequenceNo },
    /// A job arrived out of order for a pane — a bug guard signal (release).
    SeqnoGap { pane_id: PaneId },
}

/// Handle to the apply worker, owned by the `SessionManager` on the UI thread.
pub struct ApplyHandle {
    tx: Sender<ApplyJob>,
    notes: Receiver<WorkerNote>,
    join: Option<JoinHandle<()>>,
}

impl ApplyHandle {
    /// Spawn the worker thread.
    pub fn spawn() -> Self {
        let (tx, rx) = channel::<ApplyJob>();
        let (note_tx, notes) = channel::<WorkerNote>();
        let join = std::thread::Builder::new()
            .name("kmux-grid-apply".into())
            .spawn(move || run(rx, note_tx))
            .expect("spawn grid apply worker");
        Self {
            tx,
            notes,
            join: Some(join),
        }
    }

    /// A cloneable sender the per-pane facade keeps to enqueue its applies.
    pub fn sender(&self) -> Sender<ApplyJob> {
        self.tx.clone()
    }

    /// Register a fresh pane: create its publication slot, hand the worker a
    /// clone, and return the slot for the facade to read from.
    pub fn register_pane(&self, pane_id: PaneId, rows: usize, cols: usize) -> Published {
        let published: Published = Arc::new(ArcSwap::from_pointee(GridContent::new(rows, cols)));
        let _ = self.tx.send(ApplyJob::Register {
            pane_id,
            published: Arc::clone(&published),
            rows,
            cols,
        });
        published
    }

    /// Forget a pane's worker-side state.
    pub fn forget_pane(&self, pane_id: PaneId) {
        let _ = self.tx.send(ApplyJob::Forget { pane_id });
    }

    /// Drop all pane state (reconnect).
    pub fn reset(&self) {
        let _ = self.tx.send(ApplyJob::Reset);
    }

    /// Drain queued notes (effects / resyncs) for the UI thread to act on.
    pub fn try_recv_note(&self) -> Option<WorkerNote> {
        self.notes.try_recv().ok()
    }

    /// Block until the worker has applied and published everything queued so
    /// far. A synchronisation point for tests (and any caller that needs the
    /// published snapshots to reflect every enqueued job before reading).
    pub fn barrier(&self) {
        let (ack_tx, ack_rx) = sync_channel::<()>(0);
        if self.tx.send(ApplyJob::Barrier(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

impl Drop for ApplyHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(ApplyJob::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Per-pane worker state: the authoritative content and where to publish it.
struct PaneState {
    content: GridContent,
    published: Published,
    /// Next expected per-pane `order` for the contiguity assertion.
    next_order: u64,
}

impl PaneState {
    fn publish(&self) {
        self.published.store(Arc::new(self.content.clone()));
    }
}

/// The worker loop: drain a batch, apply in order, publish each touched pane
/// once.
fn run(rx: Receiver<ApplyJob>, note_tx: Sender<WorkerNote>) {
    let mut panes: HashMap<PaneId, PaneState> = HashMap::new();
    let mut touched: HashSet<PaneId> = HashSet::new();
    'main: loop {
        // A recv error means every sender is gone, so there is no more work.
        let Ok(first) = rx.recv() else { break };
        let mut batch = vec![first];
        while let Ok(job) = rx.try_recv() {
            batch.push(job);
        }
        for job in batch {
            if apply_job(job, &mut panes, &mut touched, &note_tx) {
                break 'main;
            }
        }
        for pane_id in touched.drain() {
            if let Some(ps) = panes.get(&pane_id) {
                ps.publish();
            }
        }
    }
}

/// Apply one job. Returns `true` on shutdown.
fn apply_job(
    job: ApplyJob,
    panes: &mut HashMap<PaneId, PaneState>,
    touched: &mut HashSet<PaneId>,
    note_tx: &Sender<WorkerNote>,
) -> bool {
    match job {
        ApplyJob::Register {
            pane_id,
            published,
            rows,
            cols,
        } => {
            panes.insert(
                pane_id,
                PaneState {
                    content: GridContent::new(rows, cols),
                    published,
                    next_order: 0,
                },
            );
        }
        ApplyJob::Snapshot {
            pane_id,
            order,
            snapshot,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                let effect = ps.content.apply_snapshot(*snapshot);
                send_effect(note_tx, &pane_id, effect);
                touched.insert(pane_id);
            }
        }
        ApplyJob::Diff {
            pane_id,
            order,
            diff,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                let effect = ps.content.apply_diff(*diff);
                send_effect(note_tx, &pane_id, effect);
                touched.insert(pane_id);
            }
        }
        ApplyJob::Cursor {
            pane_id,
            order,
            cursor,
            modes,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                ps.content.apply_cursor_update(cursor, modes);
                touched.insert(pane_id);
            }
        }
        ApplyJob::ScrollbackAppend {
            pane_id,
            order,
            first_index,
            lines,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                let effect = ps.content.apply_scrollback_append(first_index, lines);
                send_effect(note_tx, &pane_id, effect);
                touched.insert(pane_id);
            }
        }
        ApplyJob::HistoryLines {
            pane_id,
            order,
            first_index,
            lines,
            history_total,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                ps.content
                    .apply_history_lines(first_index, lines, history_total);
                touched.insert(pane_id);
            }
        }
        ApplyJob::Clear { pane_id, order } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                let effect = ps.content.clear();
                send_effect(note_tx, &pane_id, effect);
                touched.insert(pane_id);
            }
        }
        ApplyJob::Resize {
            pane_id,
            order,
            rows,
            cols,
        } => {
            if let Some(ps) = panes.get_mut(&pane_id) {
                check_order(ps, &pane_id, order, note_tx);
                let effect = ps.content.resize(rows, cols);
                send_effect(note_tx, &pane_id, effect);
                touched.insert(pane_id);
            }
        }
        ApplyJob::Digest {
            pane_id,
            seqno,
            hash,
        } => {
            if let Some(ps) = panes.get(&pane_id)
                && ps.content.pending_history_gap().is_none()
                && ps.content.live_digest() != hash
            {
                let _ = note_tx.send(WorkerNote::DigestMismatch { pane_id, seqno });
            }
        }
        ApplyJob::Forget { pane_id } => {
            panes.remove(&pane_id);
            touched.remove(&pane_id);
        }
        ApplyJob::Reset => {
            panes.clear();
            touched.clear();
        }
        ApplyJob::Barrier(ack) => {
            // Publish everything pending so the acker observes it, then ack.
            for pane_id in touched.drain() {
                if let Some(ps) = panes.get(&pane_id) {
                    ps.publish();
                }
            }
            let _ = ack.send(());
        }
        ApplyJob::Shutdown => return true,
    }
    false
}

/// Assert per-pane jobs arrive contiguously (no reorder/loss). The mpsc channel
/// guarantees order; a violation means a logic bug upstream.
fn check_order(ps: &mut PaneState, pane_id: &str, order: u64, note_tx: &Sender<WorkerNote>) {
    debug_assert_eq!(
        order, ps.next_order,
        "apply worker saw out-of-order job for pane {pane_id}: got {order}, expected {}",
        ps.next_order
    );
    if order != ps.next_order {
        warn!(
            pane_id,
            got = order,
            expected = ps.next_order,
            "apply order gap"
        );
        let _ = note_tx.send(WorkerNote::SeqnoGap {
            pane_id: pane_id.to_string(),
        });
    }
    ps.next_order = order + 1;
}

/// Forward a non-empty view effect to the UI thread, in order.
fn send_effect(note_tx: &Sender<WorkerNote>, pane_id: &str, effect: ApplyEffect) {
    if effect.reset_view || effect.scrollback_fixup.is_some() {
        let _ = note_tx.send(WorkerNote::Effect {
            pane_id: pane_id.to_string(),
            effect,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_state() -> PaneState {
        PaneState {
            content: GridContent::new(2, 2),
            published: Arc::new(ArcSwap::from_pointee(GridContent::new(2, 2))),
            next_order: 0,
        }
    }

    #[test]
    fn check_order_advances_for_contiguous_jobs() {
        let (note_tx, note_rx) = channel();
        let mut ps = pane_state();
        for order in 0..3 {
            check_order(&mut ps, "p", order, &note_tx);
        }
        assert_eq!(ps.next_order, 3);
        assert!(
            note_rx.try_recv().is_err(),
            "no order-gap notes for contiguous jobs"
        );
    }

    #[test]
    #[should_panic(expected = "out-of-order")]
    fn check_order_trips_on_gap_in_debug() {
        let (note_tx, _note_rx) = channel();
        let mut ps = pane_state();
        check_order(&mut ps, "p", 0, &note_tx);
        // Skipping order 1 must trip the contiguity assertion.
        check_order(&mut ps, "p", 2, &note_tx);
    }

    #[test]
    fn publish_round_trips_authoritative_content() {
        let published: Published = Arc::new(ArcSwap::from_pointee(GridContent::new(4, 4)));
        let mut ps = PaneState {
            content: GridContent::new(4, 4),
            published: Arc::clone(&published),
            next_order: 0,
        };
        ps.content.resize(2, 3);
        ps.publish();
        let loaded = published.load_full();
        assert_eq!(loaded.rows, 2);
        assert_eq!(loaded.cols, 3);
        assert_eq!(loaded.cells().len(), 6);
    }
}
