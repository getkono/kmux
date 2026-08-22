//! Replies to things this client asked for, plus the two frames that are
//! pure protocol bookkeeping.

use super::*;

impl SessionManager {
    /// Handle a `ProcessOverviewResult` frame.
    pub(super) fn on_process_overview_result(
        &mut self,
        panes: Vec<PaneProcesses>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.process_overview = panes;
        events.push(SessionEvent::ProcessOverviewReceived);
        events
    }

    /// Handle a `Error` frame.
    pub(super) fn on_error(&mut self, message: String) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.status_msg = format!("Error: {message}");
        events.push(SessionEvent::ServerError { message });
        events
    }

    /// Handle a `DirectoryListing` frame.
    pub(super) fn on_directory_listing(
        &mut self,
        request_id: RequestId,
        path: String,
        parent: Option<String>,
        entries: Vec<DirEntry>,
        error: Option<String>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        // Drop stale replies: only the most recent request counts (the
        // user may have navigated again before this listing returned).
        if self.pending_dir_request != Some(request_id) {
            return events;
        }
        self.pending_dir_request = None;
        self.dir_listing = Some(DirListing {
            path,
            parent,
            entries,
            error,
        });
        events.push(SessionEvent::DirectoryListed);
        events
    }

    /// Handle a `Ping` frame.
    pub(super) fn on_ping(&mut self, seq: u64) -> Vec<SessionEvent> {
        self.send_ws(ClientMessage::Pong { seq });
        Vec::new()
    }

    /// Response to a client-initiated Ping; the liveness tracker
    /// also refreshes on this (in addition to the general
    /// `observe_inbound` at the top of this fn) so outstanding
    /// ping seqs are cleared.
    pub(super) fn on_pong(&mut self, seq: u64) -> Vec<SessionEvent> {
        if let Some(rtt) = self.liveness.on_pong(seq, Instant::now()) {
            let rtt_ms = rtt.as_secs_f64() * 1000.0;
            self.metrics.observe_rtt(rtt_ms);
            self.record_rtt_to_supervisor(rtt_ms);
        }
        Vec::new()
    }

    /// Reply to `ClientMessage::Notify` (issue #169). Consumed directly by
    /// the `kmux notify` CLI's own read loop, not the streaming session
    /// manager — ignore here for exhaustiveness.
    pub(super) fn on_notify_accepted() -> Vec<SessionEvent> {
        Vec::new()
    }

    /// Replies to `ClientMessage::FetchLogs` (issue #187). Consumed by the
    /// `kmux daemon logs --server` CLI read loop, never by the GUI session
    /// manager — ignore here for exhaustiveness.
    pub(super) fn on_log_chunk() -> Vec<SessionEvent> {
        Vec::new()
    }
}
