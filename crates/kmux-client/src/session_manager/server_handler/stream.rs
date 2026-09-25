//! The grid pipeline: snapshots, diffs, the cursor, and the sequence
//! bookkeeping that decides when a pane has fallen behind and must resync.

use super::*;

impl SessionManager {
    /// Handle a `TerminalSnapshot` frame.
    pub(super) fn on_terminal_snapshot(
        &mut self,
        pane_id: PaneId,
        snapshot: Arc<GridSnapshot>,
        seqno: SequenceNo,
        sent_at_ms: u64,
    ) -> Vec<SessionEvent> {
        let start = Instant::now();
        let grid = self.ensure_pane(&pane_id);
        // The client owns its freshly-decoded Arc (refcount 1), so this
        // moves the grid out rather than cloning it.
        grid.apply_snapshot(Arc::unwrap_or_clone(snapshot));
        self.mark_synced(pane_id, seqno, start, sent_at_ms);
        Vec::new()
    }

    /// Handle a `TerminalUpdate` frame.
    pub(super) fn on_terminal_update(
        &mut self,
        pane_id: &PaneId,
        diff: Arc<TerminalDiff>,
        seqno: SequenceNo,
        sent_at_ms: u64,
    ) -> Vec<SessionEvent> {
        let events = Vec::new();
        if !self.check_pane_sync(pane_id, seqno) {
            return events;
        }
        let start = Instant::now();
        let diff = Arc::unwrap_or_clone(diff);
        let op_count = diff.ops.len();
        if let Some(grid) = self.buffers.get_mut(pane_id) {
            grid.apply_diff(diff);
            self.metrics.record_diff_stats(op_count);
        }
        self.mark_synced(pane_id.clone(), seqno, start, sent_at_ms);
        if op_count > 100 {
            let net_apply_ms = epoch_millis().saturating_sub(sent_at_ms) as f64;
            self.metrics.record_large_diff(net_apply_ms);
        }
        self.maybe_fetch_history(pane_id);
        events
    }

    /// Handle a `CursorUpdate` frame.
    pub(super) fn on_cursor_update(
        &mut self,
        pane_id: PaneId,
        cursor: CursorState,
        modes: TermModes,
        seqno: SequenceNo,
        sent_at_ms: u64,
    ) -> Vec<SessionEvent> {
        let events = Vec::new();
        if !self.check_pane_sync(&pane_id, seqno) {
            return events;
        }
        let start = Instant::now();
        if let Some(grid) = self.buffers.get_mut(&pane_id) {
            grid.apply_cursor_update(cursor, modes);
        }
        self.mark_synced(pane_id, seqno, start, sent_at_ms);
        events
    }

    /// Handle a `SyncReset` frame.
    pub(super) fn on_sync_reset(&mut self, pane_id: PaneId) -> Vec<SessionEvent> {
        if let Some(grid) = self.buffers.get_mut(&pane_id) {
            grid.clear();
        }
        self.in_flight_history_fetches.remove(&pane_id);
        self.metrics.record_resync(&pane_id, "server sync reset");
        self.pane_sync.insert(pane_id, PaneSync::AwaitingSync);
        Vec::new()
    }

    /// Handle a `GridDigest` frame.
    pub(super) fn on_grid_digest(
        &mut self,
        pane_id: PaneId,
        seqno: SequenceNo,
        hash: u128,
    ) -> Vec<SessionEvent> {
        // The digest certifies the grid as of `seqno`. Only verify when
        // the pane is synced at EXACTLY that seqno (its next-expected is
        // `seqno + 1`); otherwise the client is mid-stream, resyncing, or
        // the digest is stale, and a comparison would be meaningless. The
        // digest carries no new seqno and never advances sync state — it
        // is a pure side-band check. Skip while a lazy `FetchHistory` is
        // outstanding: the client is legitimately behind on the envelope
        // counts the digest covers, so a mismatch would be a false alarm.
        let synced_here = matches!(
            self.pane_sync.get(&pane_id),
            Some(PaneSync::Synced { expected }) if expected.0 == seqno.0 + 1
        );
        if synced_here {
            // A `Published` pane's content lives on the apply worker, so
            // the digest is checked there (in-order with the data it
            // certifies) and a mismatch returns via `WorkerNote`, handled
            // in `drain_apply_notes`. A `Local` pane checks inline and
            // returns `Some(mismatch)`.
            let inline_mismatch = self
                .buffers
                .get_mut(&pane_id)
                .and_then(|grid| grid.request_digest_check(seqno, hash));
            if inline_mismatch == Some(true) {
                self.metrics.record_digest_mismatch(&pane_id, seqno.0);
                self.metrics.record_resync(&pane_id, "grid digest mismatch");
                if let Some(grid) = self.buffers.get_mut(&pane_id) {
                    grid.clear();
                }
                self.in_flight_history_fetches.remove(&pane_id);
                self.attach_fresh(pane_id);
            }
        }
        Vec::new()
    }

    /// Handle a `Lagged` frame.
    pub(super) fn on_lagged(&mut self, pane_id: PaneId, missed_count: u64) -> Vec<SessionEvent> {
        self.metrics.record_lag(&pane_id, missed_count);
        self.metrics.record_resync(&pane_id, "lagged");
        if let Some(grid) = self.buffers.get_mut(&pane_id) {
            grid.clear();
        }
        self.in_flight_history_fetches.remove(&pane_id);
        self.attach_fresh(pane_id);
        Vec::new()
    }

    /// Handle a `ScrollbackAppend` frame.
    pub(super) fn on_scrollback_append(
        &mut self,
        pane_id: &PaneId,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
        seqno: SequenceNo,
        sent_at_ms: u64,
    ) -> Vec<SessionEvent> {
        let events = Vec::new();
        if !self.check_pane_sync(pane_id, seqno) {
            return events;
        }
        let start = Instant::now();
        if let Some(grid) = self.buffers.get_mut(pane_id) {
            grid.apply_scrollback_append(first_index, lines);
        }
        self.mark_synced(pane_id.clone(), seqno, start, sent_at_ms);
        self.maybe_fetch_history(pane_id);
        events
    }

    /// Handle a `HistoryLines` frame.
    pub(super) fn on_history_lines(
        &mut self,
        request_id: RequestId,
        pane_id: &str,
        first_index: u64,
        lines: Vec<ScrollbackLine>,
        history_total: u64,
    ) -> Vec<SessionEvent> {
        if let Some(grid) = self.buffers.get_mut(pane_id) {
            grid.apply_history_lines(first_index, lines, history_total);
        }
        // Clear only if this reply matches the in-flight request for
        // this pane; otherwise a stale reply from a prior attach could
        // unblock a fresher request.
        if self
            .in_flight_history_fetches
            .get(pane_id)
            .is_some_and(|rid| *rid == request_id)
        {
            self.in_flight_history_fetches.remove(pane_id);
        }
        self.maybe_fetch_history(pane_id);
        Vec::new()
    }
}
