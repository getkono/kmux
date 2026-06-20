use kmux_protocol::messages::{ClientCapabilities, ClientId, PaneInfo};

use crate::grid::CellGrid;

use super::SessionManager;

impl SessionManager {
    // ── Getters ───────────────────────────────────────────────────────────────

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Active session word_id.
    pub fn active_session(&self) -> Option<&str> {
        self.active_session.as_deref()
    }

    /// Active pane_id.
    pub fn active_pane_id(&self) -> Option<&str> {
        self.active_pane.as_deref()
    }

    pub fn session_list(&self) -> &[kmux_protocol::messages::SessionEntry] {
        &self.session_list
    }

    /// Latest per-pane process trees (issue #122), as last delivered by the
    /// daemon. Empty until the first `ProcessOverviewResult` arrives.
    pub fn process_overview(&self) -> &[kmux_protocol::messages::PaneProcesses] {
        &self.process_overview
    }

    pub fn buffer(&self, pane_id: &str) -> Option<&CellGrid> {
        self.buffers.get(pane_id)
    }

    pub fn buffer_mut(&mut self, pane_id: &str) -> Option<&mut CellGrid> {
        self.buffers.get_mut(pane_id)
    }

    pub fn active_grid(&self) -> Option<&CellGrid> {
        self.active_pane.as_ref().and_then(|p| self.buffers.get(p))
    }

    pub fn active_grid_mut(&mut self) -> Option<&mut CellGrid> {
        if let Some(pane_id) = &self.active_pane {
            let pane_id = pane_id.clone();
            self.buffers.get_mut(&pane_id)
        } else {
            None
        }
    }

    pub fn status_msg(&self) -> &str {
        &self.status_msg
    }

    pub fn set_status_msg(&mut self, msg: String) {
        self.status_msg = msg;
    }

    pub fn host_port_display(&self) -> String {
        if self.connected {
            format!("{}:{}", self.host, self.port)
        } else if !self.last_host.is_empty() {
            format!("{}:{}", self.last_host, self.last_port)
        } else {
            String::new()
        }
    }

    pub fn active_term_size(&self) -> Option<(u16, u16)> {
        self.active_grid().map(|b| (b.rows as u16, b.cols as u16))
    }

    pub fn is_input_locked(&self, pane_id: &str) -> bool {
        self.input_locked.get(pane_id).copied().unwrap_or(false)
    }

    pub fn active_input_locked(&self) -> bool {
        self.active_pane
            .as_ref()
            .map(|p| self.is_input_locked(p))
            .unwrap_or(false)
    }

    pub fn client_id(&self) -> Option<ClientId> {
        self.client_id
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn capabilities(&self) -> &ClientCapabilities {
        &self.capabilities
    }

    pub fn accept_invalid_certs(&self) -> bool {
        self.accept_invalid_certs
    }

    /// EWMA round-trip time (ms) for the active transport — the HUD's network
    /// latency counter (issue #61). `None` before the first Ping/Pong.
    pub fn last_rtt_ms(&self) -> Option<f64> {
        let key = self.metrics.active_transport()?;
        self.metrics.rtt.summary(key).and_then(|s| s.ewma_ms)
    }

    /// Whether the link has gone quiet for longer than 3× the ping interval
    /// (issue #61: stars the latency counter). False when not connected.
    pub fn is_ping_stale(&self, now: std::time::Instant) -> bool {
        self.connection_state.is_live()
            && self.liveness.idle_since(now) > 3 * crate::liveness::PING_INTERVAL
    }

    /// The currently-active transport channel (QUIC/TCP+TLS/UDS/TCP).
    pub fn current_transport(&self) -> kmux_protocol::messages::TransportKind {
        self.current_transport
    }

    /// RTT summary (EWMA + recent avg/max + sample count) for the active
    /// transport, or `None` before the first Ping/Pong round-trip. Drives the
    /// connection inspector and the HUD latency counter.
    pub fn active_rtt(&self) -> Option<crate::metrics::RttSummary> {
        let key = self.metrics.active_transport()?;
        self.metrics.rtt.summary(key)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn active_session_panes(&self) -> &[PaneInfo] {
        self.active_session
            .as_ref()
            .and_then(|wid| self.session_list.iter().find(|e| e.meta.word_id == *wid))
            .map(|e| e.panes.as_slice())
            .unwrap_or(&[])
    }

    /// The latest directory listing received from the daemon (in response to
    /// [`SessionManager::request_list_directory`]), or `None` if none has
    /// arrived yet. Backs the app-layer directory browser.
    pub fn dir_listing(&self) -> Option<&super::DirListing> {
        self.dir_listing.as_ref()
    }

    /// Test-only: the `request_id` of the in-flight `ListDirectory`, so a test
    /// in a dependent crate can synthesize the matching `DirectoryListing`
    /// reply (the field itself is crate-private).
    #[doc(hidden)]
    pub fn pending_dir_request_for_test(&self) -> Option<u64> {
        self.pending_dir_request
    }
}
