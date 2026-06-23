mod accessors;
mod connection;
mod input;
mod server_handler;
mod session_ops;
mod tabs;

pub use server_handler::SessionEvent;

use std::collections::HashMap;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientInfo, ClientMessage, PaneId, PaneProcesses, SequenceNo,
    SessionEntry, TermSize, WordId,
};
use tokio::sync::mpsc;
use tracing::warn;

use kmux_protocol::messages::ConnectionId;

use crate::connection_state::ConnectionState;
use crate::grid::CellGrid;
use crate::liveness::Liveness;
use crate::metrics::{JsonlSink, MetricsStore};
use crate::supervisor::RttSample;
use crate::transport::TransportKind;

/// Per-pane synchronisation state.
#[derive(Default)]
pub(super) enum PaneSync {
    Synced {
        expected: SequenceNo,
    },
    #[default]
    AwaitingSync,
}

/// The latest directory listing received from the daemon in response to a
/// [`SessionManager::request_list_directory`] call. Backs the app-layer
/// directory browser (choosing where to open a new session on a possibly
/// remote daemon).
#[derive(Debug, Clone, Default)]
pub struct DirListing {
    /// The canonicalized directory actually listed.
    pub path: String,
    /// Its parent directory, or `None` at the filesystem root.
    pub parent: Option<String>,
    /// Its subdirectories (directories only).
    pub entries: Vec<kmux_protocol::messages::DirEntry>,
    /// A human-readable message when the listing failed; `None` on success.
    pub error: Option<String>,
}

/// Shared client-side session management logic used by the client frontends.
pub struct SessionManager {
    // Connection params
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) token: String,
    pub(super) accept_invalid_certs: bool,
    /// Rendering capabilities declared to the daemon at Auth time.
    pub(super) capabilities: ClientCapabilities,

    // Live connection
    pub(super) ws_sender: Option<mpsc::UnboundedSender<ClientMessage>>,
    pub connected: bool,
    pub status_msg: String,

    // Session / tab / pane state
    pub session_list: Vec<SessionEntry>,
    /// Latest per-pane process trees from the daemon (issue #122), keyed
    /// implicitly by `PaneProcesses::pane_id`. Refreshed by
    /// [`Self::request_process_overview`] while the process-overview view is
    /// open; empty otherwise. Joined with `session_list` by the app layer.
    pub process_overview: Vec<PaneProcesses>,
    /// Connections attached to the session most recently queried via
    /// [`Self::request_client_list`] (issue #146); refreshed while the
    /// connected-clients view is open, empty otherwise. The `word_id` it pertains
    /// to is [`Self::client_list_word`].
    pub client_list: Vec<ClientInfo>,
    /// The session `word_id` that [`Self::client_list`] pertains to, if any.
    pub client_list_word: Option<WordId>,
    /// This connection's own cryptographic identity fingerprint, from
    /// `AuthResult` (issue #146).
    pub machine_id: Option<String>,
    /// This connection's daemon-assigned user-readable label, from `AuthResult`.
    pub label: Option<String>,
    /// The daemon's own identity fingerprint, from `AuthResult`.
    pub server_machine_id: Option<String>,
    /// Currently active session (word_id).
    pub active_session: Option<WordId>,
    /// The tab currently viewed within `active_session` (client-local — which
    /// tab a client views is not shared, unlike the layout tree + focus).
    pub active_tab: Option<u32>,
    /// The **focused** pane (pane_id = "{word_id}/{pane_index}") — the input
    /// target and the highlighted leaf within the visible set.
    pub active_pane: Option<PaneId>,
    /// Panes currently attached + rendered: the leaves of the active tab's
    /// layout. `active_pane` is the focused one within this set.
    pub(super) visible_panes: Vec<PaneId>,
    /// Per-pane content size (each pane's resolved sub-rect), pushed by the
    /// frontend from the shared layout resolver. Attach/Resize use this; falls
    /// back to the full window size when unset (single-pane tabs).
    pub(super) pane_sizes: HashMap<PaneId, TermSize>,
    /// A pane the client created and wants to focus once the refreshed session
    /// list (carrying its new tab) arrives.
    pub(super) pending_select_pane: Option<PaneId>,
    /// Client-local view flag: when set, only the focused pane is rendered/sized
    /// (full content area), à la tmux zoom. Does not mutate the shared tree.
    pub(super) zoomed: bool,
    /// Rotating index into the preset [`LayoutScheme`]s for `cycle_layout`.
    pub(super) layout_scheme_idx: usize,
    /// Terminal buffers keyed by pane_id.
    pub buffers: HashMap<PaneId, CellGrid>,
    pub(super) pane_sync: HashMap<PaneId, PaneSync>,
    pub input_locked: HashMap<PaneId, bool>,
    pub(super) next_request_id: u64,
    /// Panes with an outstanding `FetchHistory` request; maps `pane_id` to the
    /// `request_id` of the in-flight query so we can coalesce (never issue a
    /// second request while the first is pending) and reconcile responses.
    pub(super) in_flight_history_fetches: HashMap<PaneId, u64>,
    /// `request_id` of the most recent in-flight `ListDirectory` request, used
    /// to drop stale `DirectoryListing` replies (the user may navigate again
    /// before a slow listing returns). `None` when no request is outstanding.
    pub(super) pending_dir_request: Option<u64>,
    /// The latest directory listing received from the daemon, surfaced to the
    /// app-layer directory browser.
    pub(super) dir_listing: Option<DirListing>,
    pub client_id: Option<ClientId>,

    // Observability
    pub metrics: MetricsStore,

    // Last-successful connection info for display / reconnect
    pub(super) last_host: String,
    pub(super) last_port: u16,

    /// Server binary version reported in `AuthResult`; populated on successful auth.
    pub server_version: Option<String>,

    /// Connection identity assigned by the server. Persists across transport switches
    /// (QUIC ↔ TCP) so the daemon can transfer pane attachments to the new channel.
    pub connection_id: Option<ConnectionId>,

    /// The active transport kind (QUIC or TCP).
    pub current_transport: TransportKind,

    /// Last terminal size reported by the client (rows/cols after UI chrome subtraction).
    /// Sent with every `Attach` so the daemon can apply smallest-wins negotiation.
    pub(super) last_term_size: TermSize,

    /// High-level connection state surfaced to the TUI badge + overlay.
    /// `connected` and `status_msg` are derived from this on every transition.
    pub(super) connection_state: ConnectionState,

    /// Tracks inbound/outbound ping traffic so we can declare the
    /// connection dead proactively when the server stops responding.
    pub(super) liveness: Liveness,

    /// Optional sink for RTT observations. Set by the caller when a
    /// `TransportSupervisor` is spawned so the scorer operates on live
    /// measurements. `None` when no supervisor exists (direct QUIC path).
    pub(super) rtt_tx: Option<mpsc::UnboundedSender<RttSample>>,

    /// Transport override (issue #69). `Some(kind)` pins the transport and
    /// disables the supervisor's periodic heuristic; `None` is auto mode. This
    /// is the source of truth (persists across reconnects); it is re-seeded
    /// into each freshly-spawned supervisor and pushed live via `override_tx`.
    pub(super) transport_override: Option<TransportKind>,
    /// Live channel to the current supervisor's override receiver, if one is
    /// running. Replaced on every supervisor spawn via [`Self::set_override_sink`].
    pub(super) override_tx: Option<mpsc::UnboundedSender<Option<TransportKind>>>,
}

impl SessionManager {
    pub fn new(
        host: String,
        port: u16,
        token: String,
        accept_invalid_certs: bool,
        capabilities: ClientCapabilities,
    ) -> Self {
        Self {
            last_host: host.clone(),
            last_port: port,
            host,
            port,
            token,
            accept_invalid_certs,
            capabilities,
            ws_sender: None,
            connected: false,
            status_msg: String::new(),
            session_list: Vec::new(),
            process_overview: Vec::new(),
            client_list: Vec::new(),
            client_list_word: None,
            machine_id: None,
            label: None,
            server_machine_id: None,
            active_session: None,
            active_tab: None,
            active_pane: None,
            visible_panes: Vec::new(),
            pane_sizes: HashMap::new(),
            pending_select_pane: None,
            zoomed: false,
            layout_scheme_idx: 0,
            buffers: HashMap::new(),
            pane_sync: HashMap::new(),
            input_locked: HashMap::new(),
            next_request_id: 0,
            in_flight_history_fetches: HashMap::new(),
            pending_dir_request: None,
            dir_listing: None,
            client_id: None,
            metrics: MetricsStore::in_memory(),
            server_version: None,
            connection_id: None,
            current_transport: TransportKind::Quic,
            last_term_size: TermSize::default(),
            connection_state: ConnectionState::Idle,
            liveness: Liveness::new(Instant::now()),
            rtt_tx: None,
            transport_override: None,
            override_tx: None,
        }
    }

    /// Wire the session manager to a `TransportSupervisor`'s RTT sink.
    /// Called by the caller that spawns the supervisor so RTT observations
    /// feed the scorer's EWMA.
    pub fn set_rtt_sink(&mut self, tx: mpsc::UnboundedSender<RttSample>) {
        self.rtt_tx = Some(tx);
    }

    /// Wire the session manager to a freshly-spawned `TransportSupervisor`'s
    /// override receiver (issue #69). Called alongside [`Self::set_rtt_sink`].
    pub fn set_override_sink(&mut self, tx: mpsc::UnboundedSender<Option<TransportKind>>) {
        self.override_tx = Some(tx);
    }

    /// The active transport override (`None` = auto). Drives the protocol
    /// indicator and the connection inspector.
    pub fn transport_override(&self) -> Option<TransportKind> {
        self.transport_override
    }

    /// Set (or clear, with `None`) the transport override. Remembers the choice
    /// across reconnects and pushes it live to a running supervisor so it takes
    /// effect immediately. The supervisor stops auto-probing while pinned.
    pub fn set_transport_override(&mut self, target: Option<TransportKind>) {
        self.transport_override = target;
        if let Some(tx) = self.override_tx.as_ref() {
            // The supervisor may have exited (reconnect in flight); a send
            // error just means the next spawn will re-seed from the field.
            let _ = tx.send(target);
        }
    }

    /// Forward an RTT observation to the supervisor (if any) tagged with
    /// the currently-active transport.
    pub(super) fn record_rtt_to_supervisor(&self, rtt_ms: f64) {
        if let Some(tx) = self.rtt_tx.as_ref() {
            let _ = tx.send(RttSample {
                kind: self.current_transport,
                rtt_ms,
            });
        }
    }

    /// Current connection state. This is the single source of truth for the
    /// TUI badge and the disconnect overlay.
    pub fn connection_state(&self) -> &ConnectionState {
        &self.connection_state
    }

    /// Internal transition helper. Mirrors state into `connected` and
    /// `status_msg` so older code paths keep working.
    pub(super) fn set_connection_state(&mut self, new_state: ConnectionState) {
        self.connected = new_state.is_live();
        self.status_msg = new_state.badge_label();
        self.connection_state = new_state;
    }

    /// If the outbound ping cadence has elapsed, put a `Ping` on the wire.
    /// Called by the frontend on a timer tick.
    pub fn maybe_send_client_ping(&mut self, now: Instant) {
        if !self.connection_state.is_live() {
            return;
        }
        if let Some(msg) = self.liveness.client_ping_due(now) {
            self.send_ws(msg);
        }
    }

    /// True once no inbound frame has been seen for the liveness timeout.
    pub fn is_liveness_timed_out(&self, now: Instant) -> bool {
        self.connection_state.is_live() && self.liveness.is_timed_out(now)
    }

    /// Next instant at which the frontend must wake up for ping / timeout
    /// evaluation. `None` if not currently connected.
    pub fn liveness_next_wakeup(&self) -> Option<Instant> {
        self.connection_state
            .is_live()
            .then(|| self.liveness.next_wakeup())
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    pub(super) fn send_ws(&mut self, msg: ClientMessage) {
        let Some(tx) = self.ws_sender.as_ref() else {
            return;
        };
        let bytes = kmux_protocol::encode_client(&msg)
            .map(|b| b.len())
            .unwrap_or(0);
        let category = msg.category();
        if let Err(e) = tx.send(msg) {
            warn!("send_ws failed: {e}");
            return;
        }
        if bytes > 0 {
            self.metrics.record_outbound(bytes, category);
        }
    }

    /// Enable rolling-JSONL persistence for this session's metrics. Called
    /// by the TUI after construction so tests stay filesystem-free.
    pub fn enable_metrics_persistence(&mut self) {
        match kmux_protocol::dirs::metrics_log_path() {
            Ok(path) => {
                self.metrics = MetricsStore::new(Some(JsonlSink::new(path)));
            }
            Err(e) => warn!("metrics persistence disabled: {e}"),
        }
    }

    /// The `host:port` string used as the metrics-layer address for the
    /// currently-pointed endpoint. UDS connections still key off the
    /// user-visible target so the overlay stays understandable.
    fn metrics_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Tag the metrics layer with the currently active transport.
    pub(super) fn tag_transport(&mut self, kind: TransportKind) {
        let addr = self.metrics_address();
        self.metrics.on_transport_active(kind, addr);
    }

    pub(super) fn next_rid(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Maximum number of scrollback lines a single `FetchHistory` asks for.
    /// Keeps the reply small enough to render promptly while still making
    /// good progress on catching up to the server's `history_total`.
    const FETCH_HISTORY_CHUNK: u32 = 500;

    /// If this pane has a known scrollback gap and no in-flight fetch,
    /// issue a bounded `FetchHistory` to close the gap. Coalesces: at most
    /// one request per pane is in flight at any time.
    pub(super) fn maybe_fetch_history(&mut self, pane_id: &str) {
        if self.in_flight_history_fetches.contains_key(pane_id) {
            return;
        }
        let Some(grid) = self.buffers.get(pane_id) else {
            return;
        };
        let Some((have, want)) = grid.pending_history_gap() else {
            return;
        };
        let count = (want - have).min(Self::FETCH_HISTORY_CHUNK as u64) as u32;
        if count == 0 {
            return;
        }
        let request_id = self.next_rid();
        self.in_flight_history_fetches
            .insert(pane_id.to_string(), request_id);
        self.send_ws(ClientMessage::FetchHistory {
            request_id,
            pane_id: pane_id.to_string(),
            start_index: have,
            count,
        });
    }

    pub(super) fn attach_fresh(&mut self, pane_id: String) {
        self.pane_sync
            .insert(pane_id.clone(), PaneSync::AwaitingSync);
        self.in_flight_history_fetches.remove(&pane_id);
        // Use the pane's resolved sub-rect size if the frontend has set one;
        // otherwise the full window size (correct for a single-pane tab).
        let size = self
            .pane_sizes
            .get(&pane_id)
            .copied()
            .unwrap_or(self.last_term_size);
        self.send_ws(ClientMessage::Attach {
            pane_id,
            last_seqno: None,
            size,
        });
    }

    /// Update the stored window size (the fallback) and send a `Resize` to every
    /// attached pane. A pane with a resolved sub-rect size (set by the frontend
    /// via [`Self::set_pane_sizes`] for a tiled tab) keeps that size; the rest
    /// resize to the full window (the single-pane case).
    pub fn update_term_size(&mut self, size: TermSize) {
        self.last_term_size = size;
        let attached: Vec<String> = self
            .buffers
            .keys()
            .filter(|pid| self.pane_sync.contains_key(*pid))
            .cloned()
            .collect();
        for pane_id in attached {
            let s = self.pane_sizes.get(&pane_id).copied().unwrap_or(size);
            self.send_ws(ClientMessage::Resize { pane_id, size: s });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kmux_protocol::format_pane_id;
    use kmux_protocol::messages::{
        ClientCapabilities, ClientId, ClientMessage, GridSnapshot, PaneInfo, SequenceNo,
        ServerMessage, SessionEntry, SessionMeta, SessionStatus, TermModes, TermSize,
    };
    use tokio::sync::mpsc;

    use super::{PaneSync, SessionManager};
    use crate::grid::CellGrid;

    fn make_manager() -> SessionManager {
        SessionManager::new(
            "127.0.0.1".to_string(),
            8443,
            "test-token".to_string(),
            false,
            ClientCapabilities::default(),
        )
    }

    fn make_connected_manager() -> (SessionManager, mpsc::UnboundedReceiver<ClientMessage>) {
        let mut mgr = make_manager();
        let (tx, rx) = mpsc::unbounded_channel();
        mgr.ws_sender = Some(tx);
        mgr.connected = true;
        (mgr, rx)
    }

    /// The client side of the live daemon upgrade (#36): when the daemon restarts,
    /// the old transport dies and the client reconnects to the successor. The
    /// successor adopts the predecessor's token and can transfer the existing pane
    /// streams — but only if the client re-authenticates with the SAME
    /// `connection_id`. This test pins the seam (`prepare_reconnect`) that must
    /// preserve that identity across the disconnect, so a handoff reconnect is
    /// seamless rather than starting a brand-new connection.
    #[test]
    fn reconnect_preserves_connection_id_for_handoff() {
        use kmux_protocol::messages::ConnectionId;

        use crate::connection_state::{ConnectionState, DisconnectReason};

        let (mut mgr, _rx) = make_connected_manager();
        let cid = ConnectionId(4242);
        mgr.connection_id = Some(cid);

        // Daemon went away (it restarted): the transport is lost but identity stays.
        mgr.mark_connection_lost_with(DisconnectReason::ServerClosed);
        assert!(mgr.ws_sender.is_none(), "the dead sender must be dropped");
        assert!(
            matches!(
                mgr.connection_state(),
                ConnectionState::Disconnected {
                    reason: DisconnectReason::ServerClosed
                }
            ),
            "should record why the connection dropped"
        );
        assert_eq!(
            mgr.connection_id,
            Some(cid),
            "connection_id must survive a lost connection"
        );

        // Begin the reconnect: state flips to Handshaking, identity still retained
        // so the re-auth can pass `connection_id: Some(..)` to the successor.
        mgr.prepare_reconnect();
        assert!(matches!(
            mgr.connection_state(),
            ConnectionState::Handshaking
        ));
        assert_eq!(
            mgr.connection_id,
            Some(cid),
            "prepare_reconnect must preserve connection_id so the successor can \
             transfer pane streams"
        );
    }

    fn make_entry(word_id: &str, cwd: &str) -> SessionEntry {
        use kmux_protocol::messages::SessionMeta;
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word_id.to_string(),
                name: std::path::Path::new(cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(word_id)
                    .to_string(),
                cwd: cwd.to_string(),
            },
            panes: vec![PaneInfo {
                pane_id: format_pane_id(word_id, 0),
                pane_index: 0,
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status: SessionStatus::Running,
                title: String::new(),
                progress_state: Default::default(),
                progress: None,
            }],
            tabs: vec![kmux_protocol::messages::TabInfo {
                tab_index: 0,
                name: "1".to_string(),
                layout: kmux_protocol::messages::LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        }
    }

    /// A connected manager with one active pane whose inner program has the
    /// given mouse modes set, plus the outbound message receiver.
    fn manager_with_modes(
        modes: TermModes,
    ) -> (SessionManager, mpsc::UnboundedReceiver<ClientMessage>) {
        use kmux_protocol::messages::CursorState;
        let (mut mgr, rx) = make_connected_manager();
        let mut grid = CellGrid::default();
        grid.apply_cursor_update(CursorState::default(), modes);
        mgr.buffers.insert("s1/0".to_string(), grid);
        mgr.active_pane = Some("s1/0".to_string());
        (mgr, rx)
    }

    fn mouse(
        button: crate::input::MouseButton,
        kind: crate::input::MouseEventKind,
        shift: bool,
    ) -> crate::input::MouseEvent {
        crate::input::MouseEvent {
            button,
            kind,
            col: 1,
            row: 1,
            mods: crate::input::MouseMods {
                shift,
                ..Default::default()
            },
        }
    }

    #[test]
    fn report_mouse_forwards_press_when_app_wants_it() {
        use crate::input::{MouseButton, MouseEventKind};
        let (mut mgr, mut rx) = manager_with_modes(TermModes(
            TermModes::MOUSE_REPORT_CLICK | TermModes::SGR_MOUSE,
        ));
        let sent = mgr.report_mouse(
            false,
            mouse(MouseButton::Left, MouseEventKind::Press, false),
        );
        assert!(sent, "press should forward when mouse tracking is on");
        match rx.try_recv() {
            Ok(ClientMessage::PtyInput { data, .. }) => assert_eq!(data, b"\x1b[<0;1;1M"),
            other => panic!("expected PtyInput, got {other:?}"),
        }
    }

    #[test]
    fn report_mouse_shift_bypasses_to_local_selection() {
        use crate::input::{MouseButton, MouseEventKind};
        let (mut mgr, mut rx) = manager_with_modes(TermModes(TermModes::MOUSE_REPORT_CLICK));
        let sent = mgr.report_mouse(false, mouse(MouseButton::Left, MouseEventKind::Press, true));
        assert!(!sent, "shift is the bypass key; never forward");
        assert!(rx.try_recv().is_err(), "nothing should be sent to the PTY");
    }

    #[test]
    fn report_mouse_ignored_when_no_mouse_mode() {
        use crate::input::{MouseButton, MouseEventKind};
        let (mut mgr, mut rx) = manager_with_modes(TermModes::EMPTY);
        let sent = mgr.report_mouse(
            false,
            mouse(MouseButton::Left, MouseEventKind::Press, false),
        );
        assert!(!sent, "no mouse mode → the click is ours");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn report_mouse_motion_gated_by_mode() {
        use crate::input::{MouseButton, MouseEventKind};
        let motion = || mouse(MouseButton::Left, MouseEventKind::Motion, false);

        // Click tracking (1000) reports no motion at all.
        let (mut mgr, _rx) = manager_with_modes(TermModes(TermModes::MOUSE_REPORT_CLICK));
        assert!(!mgr.report_mouse(true, motion()));

        // Button-event tracking (1002): motion only while a button is held.
        let (mut mgr, _rx) = manager_with_modes(TermModes(TermModes::MOUSE_DRAG));
        assert!(mgr.report_mouse(true, motion()));
        let (mut mgr, _rx) = manager_with_modes(TermModes(TermModes::MOUSE_DRAG));
        assert!(!mgr.report_mouse(false, motion()));

        // Any-event tracking (1003): every motion, even with no button.
        let (mut mgr, _rx) = manager_with_modes(TermModes(TermModes::MOUSE_MOTION));
        assert!(mgr.report_mouse(false, motion()));
    }

    #[test]
    fn auth_ok_sets_client_id() {
        let mut mgr = make_manager();
        let events = mgr.handle_server_message(ServerMessage::AuthResult {
            success: true,
            reason: None,
            client_id: Some(ClientId(42)),
            server_version: Some("0.1.0".to_string()),
            connection_id: None,
            compression: None,
            machine_id: None,
            label: None,
            server_machine_id: None,
        });
        use super::server_handler::SessionEvent;
        assert!(matches!(events.as_slice(), [SessionEvent::AuthOk]));
        assert_eq!(mgr.client_id, Some(ClientId(42)));
    }

    #[test]
    fn auth_failed_emits_event_and_clears_connection() {
        use super::server_handler::SessionEvent;
        let mut mgr = make_manager();
        mgr.connected = true;
        let (tx, _rx) = mpsc::unbounded_channel::<ClientMessage>();
        mgr.ws_sender = Some(tx);

        let events = mgr.handle_server_message(ServerMessage::AuthResult {
            success: false,
            reason: Some("bad token".to_string()),
            client_id: None,
            server_version: None,
            connection_id: None,
            compression: None,
            machine_id: None,
            label: None,
            server_machine_id: None,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::AuthFailed { reason }] if reason == "bad token"
        ));
        assert!(!mgr.connected);
        assert!(mgr.ws_sender.is_none());
    }

    #[test]
    fn session_list_populates_and_auto_attaches() {
        use super::server_handler::SessionEvent;
        let (mut mgr, mut rx) = make_connected_manager();

        let sessions = vec![make_entry("eagle", "/home/user/proj")];
        let events = mgr.handle_server_message(ServerMessage::SessionListResult {
            request_id: 0,
            sessions,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionListReceived]
        ));
        assert_eq!(mgr.session_list.len(), 1);
        assert_eq!(mgr.active_session.as_deref(), Some("eagle"));
        assert_eq!(mgr.active_pane.as_deref(), Some("eagle/0"));
        // Attach message should have been sent
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn process_overview_result_caches_panes() {
        use super::server_handler::SessionEvent;
        use kmux_protocol::messages::{PaneProcesses, ProcessSample};
        let (mut mgr, _rx) = make_connected_manager();

        let panes = vec![PaneProcesses {
            pane_id: "eagle/0".into(),
            root_pid: Some(100),
            processes: vec![ProcessSample {
                pid: 100,
                ppid: None,
                name: "zsh".into(),
                cmd: "-zsh".into(),
                cpu_percent: 1.0,
                mem_bytes: 1024,
            }],
        }];
        let events = mgr.handle_server_message(ServerMessage::ProcessOverviewResult {
            request_id: 7,
            panes: panes.clone(),
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::ProcessOverviewReceived]
        ));
        assert_eq!(mgr.process_overview(), panes.as_slice());
    }

    #[test]
    fn session_created_switches_active() {
        use super::server_handler::SessionEvent;
        let (mut mgr, _rx) = make_connected_manager();
        let entry = make_entry("falcon", "/home/user/other");
        let events = mgr.handle_server_message(ServerMessage::SessionCreated {
            request_id: 0,
            entry,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionCreated { word_id }] if word_id == "falcon"
        ));
        assert_eq!(mgr.active_session.as_deref(), Some("falcon"));
        assert_eq!(mgr.active_pane.as_deref(), Some("falcon/0"));
        assert!(mgr.buffers.contains_key("falcon/0"));
    }

    /// Build a `SessionEntry` with one tab per pane index (mirrors the
    /// `PaneCreate` = "new tab" model after the server wraps each pane).
    fn make_entry_with_tabs(word_id: &str, cwd: &str, pane_count: u32) -> SessionEntry {
        use kmux_protocol::messages::{LayoutNode, SessionMeta, TabInfo};
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word_id.to_string(),
                name: std::path::Path::new(cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(word_id)
                    .to_string(),
                cwd: cwd.to_string(),
            },
            panes: (0..pane_count)
                .map(|i| PaneInfo {
                    pane_id: format_pane_id(word_id, i),
                    pane_index: i,
                    program: String::new(),
                    size: TermSize::default(),
                    attached_clients: vec![],
                    status: SessionStatus::Running,
                    title: String::new(),
                    progress_state: Default::default(),
                    progress: None,
                })
                .collect(),
            tabs: (0..pane_count)
                .map(|i| TabInfo {
                    tab_index: i,
                    name: format!("{}", i + 1),
                    layout: LayoutNode::single(i),
                    focused_pane: i,
                })
                .collect(),
            active_tab: 0,
            peer: None,
        }
    }

    #[test]
    fn pane_created_defers_select_until_refresh() {
        use super::server_handler::SessionEvent;
        let (mut mgr, mut rx) = make_connected_manager();

        mgr.session_list
            .push(make_entry_with_tabs("eagle", "/home/user/proj", 1));
        mgr.select_session("eagle".to_string());
        while rx.try_recv().is_ok() {}

        // PaneCreate reply: the new pane is buffered, a refresh is requested, and
        // selection is deferred (active_pane unchanged for now).
        let events = mgr.handle_server_message(ServerMessage::PaneCreated {
            request_id: 0,
            pane_id: "eagle/1".to_string(),
            session_word_id: "eagle".to_string(),
            size: TermSize::default(),
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::PaneCreated { pane_id }] if pane_id == "eagle/1"
        ));
        assert!(mgr.buffers.contains_key("eagle/1"));
        assert_eq!(mgr.pending_select_pane.as_deref(), Some("eagle/1"));
        let msgs: Vec<ClientMessage> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ClientMessage::SessionList { .. })),
            "a session-list refresh must be requested: {msgs:?}",
        );

        // The refreshed list carries the new tab (tab 1 → pane eagle/1). Now the
        // deferred select fires: switch to tab 1, attaching eagle/1 and detaching
        // the old visible set (eagle/0).
        mgr.handle_server_message(ServerMessage::SessionListResult {
            request_id: 0,
            sessions: vec![make_entry_with_tabs("eagle", "/home/user/proj", 2)],
        });
        assert_eq!(mgr.active_pane.as_deref(), Some("eagle/1"));
        assert_eq!(mgr.active_tab, Some(1));
        assert!(mgr.pending_select_pane.is_none());
        let msgs: Vec<ClientMessage> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            msgs.iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/1")
            ),
            "new pane must be attached: {msgs:?}",
        );
        assert!(
            msgs.iter()
                .any(|m| matches!(m, ClientMessage::Detach { pane_id } if pane_id == "eagle/0")),
            "previous tab's pane must be detached: {msgs:?}",
        );
    }

    #[test]
    fn layout_update_attaches_split_pane_without_detaching_sibling() {
        use kmux_protocol::messages::{LayoutNode, SplitDir};
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.session_list
            .push(make_entry_with_tabs("eagle", "/proj", 1));
        mgr.select_session("eagle".to_string());
        while rx.try_recv().is_ok() {}

        // A split adds pane 1 alongside pane 0 in the active tab (tab 0).
        mgr.handle_server_message(ServerMessage::LayoutUpdate {
            word_id: "eagle".to_string(),
            tab_index: 0,
            layout: LayoutNode::Split {
                dir: SplitDir::Horizontal,
                ratios: vec![500, 500],
                children: vec![
                    LayoutNode::Leaf { pane_index: 0 },
                    LayoutNode::Leaf { pane_index: 1 },
                ],
            },
            focused_pane: 1,
        });

        // Both panes are now visible; focus follows the server (pane 1). The
        // existing sibling (eagle/0) is not detached.
        assert_eq!(
            mgr.visible_panes(),
            &["eagle/0".to_string(), "eagle/1".to_string()]
        );
        assert_eq!(mgr.active_pane.as_deref(), Some("eagle/1"));
        let msgs: Vec<ClientMessage> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            msgs.iter().any(
                |m| matches!(m, ClientMessage::Attach { pane_id, .. } if pane_id == "eagle/1")
            ),
            "the new split pane must be attached: {msgs:?}",
        );
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, ClientMessage::Detach { pane_id } if pane_id == "eagle/0")),
            "the existing sibling must NOT be detached: {msgs:?}",
        );
    }

    #[test]
    fn session_closed_removes_and_falls_back() {
        use super::server_handler::SessionEvent;
        let (mut mgr, _rx) = make_connected_manager();

        let e1 = make_entry("s1", "/a");
        let e2 = make_entry("s2", "/b");
        mgr.session_list.push(e1);
        mgr.session_list.push(e2);
        mgr.buffers.insert("s1/0".to_string(), CellGrid::default());
        mgr.buffers.insert("s2/0".to_string(), CellGrid::default());
        mgr.active_session = Some("s1".to_string());
        mgr.active_pane = Some("s1/0".to_string());

        let events = mgr.handle_server_message(ServerMessage::SessionClosed {
            request_id: 0,
            word_id: "s1".to_string(),
            exit_code: None,
        });
        assert!(matches!(
            events.as_slice(),
            [SessionEvent::SessionClosed { word_id }] if word_id == "s1"
        ));
        assert!(!mgr.buffers.contains_key("s1/0"));
        assert_eq!(mgr.active_session.as_deref(), Some("s2"));
    }

    #[test]
    fn terminal_snapshot_transitions_to_synced() {
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let snapshot = GridSnapshot {
            rows: 24,
            cols: 80,
            cells: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_base: 0,
            scrollback_tail: Vec::new(),
        };
        mgr.handle_server_message(ServerMessage::TerminalSnapshot {
            pane_id: "eagle/0".to_string(),
            snapshot,
            seqno: SequenceNo(5),
            sent_at_ms: 0,
        });

        assert!(matches!(
            mgr.pane_sync.get("eagle/0"),
            Some(PaneSync::Synced {
                expected: SequenceNo(6)
            })
        ));
    }

    #[test]
    fn terminal_update_discarded_when_awaiting_sync() {
        use kmux_protocol::messages::TerminalDiff;
        let (mut mgr, _rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let diff = Arc::new(TerminalDiff {
            ops: vec![],
            cursor: Default::default(),
            modes: TermModes::EMPTY,
            history_total: 0,
            scrollback_reset: None,
        });
        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff,
            seqno: SequenceNo(0),
            sent_at_ms: 0,
        });

        assert_eq!(mgr.metrics.snapshot(false).counters.stale_discards, 1);
    }

    #[test]
    fn cycle_session_wraps_around() {
        let (mut mgr, _rx) = make_connected_manager();
        for (wid, cwd) in [("a", "/a"), ("b", "/b"), ("c", "/c")] {
            let entry = make_entry(wid, cwd);
            mgr.buffers
                .insert(format_pane_id(wid, 0), CellGrid::default());
            mgr.session_list.push(entry);
        }
        mgr.active_session = Some("c".to_string());
        mgr.active_pane = Some("c/0".to_string());
        mgr.cycle_session(1);
        assert_eq!(mgr.active_session.as_deref(), Some("a")); // wraps from c to a
    }

    #[test]
    fn set_layout_ratios_sends_for_active_tab() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.active_session = Some("eagle".to_string());
        mgr.active_tab = Some(0);
        mgr.set_layout_ratios(vec![1], vec![550, 450]);
        match rx.try_recv() {
            Ok(ClientMessage::SetLayoutRatios {
                word_id,
                tab_index,
                path,
                ratios,
            }) => {
                assert_eq!(word_id, "eagle");
                assert_eq!(tab_index, 0);
                assert_eq!(path, vec![1]);
                assert_eq!(ratios, vec![550, 450]);
            }
            other => panic!("expected SetLayoutRatios, got {other:?}"),
        }
    }

    #[test]
    fn set_layout_ratios_noop_without_active_tab() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.active_session = Some("eagle".to_string());
        mgr.active_tab = None;
        mgr.set_layout_ratios(vec![], vec![500, 500]);
        assert!(rx.try_recv().is_err(), "no message without an active tab");
    }

    #[test]
    fn swap_focused_sends_pane_swap_for_neighbor() {
        use kmux_protocol::messages::{LayoutNode, SplitDir, TabInfo};
        let (mut mgr, mut rx) = make_connected_manager();
        let mut entry = make_entry("eagle", "/p");
        entry.tabs = vec![TabInfo {
            tab_index: 0,
            name: "1".into(),
            layout: LayoutNode::Split {
                dir: SplitDir::Horizontal,
                ratios: vec![500, 500],
                children: vec![
                    LayoutNode::Leaf { pane_index: 0 },
                    LayoutNode::Leaf { pane_index: 1 },
                ],
            },
            focused_pane: 0,
        }];
        mgr.session_list.push(entry);
        mgr.active_session = Some("eagle".into());
        mgr.active_tab = Some(0);
        mgr.active_pane = Some("eagle/0".into());
        mgr.swap_focused(1);
        match rx.try_recv() {
            Ok(ClientMessage::PaneSwap {
                word_id,
                tab_index,
                a,
                b,
            }) => {
                assert_eq!(word_id, "eagle");
                assert_eq!(tab_index, 0);
                assert_eq!((a, b), (0, 1));
            }
            other => panic!("expected PaneSwap, got {other:?}"),
        }
    }

    #[test]
    fn swap_focused_noop_for_single_pane_tab() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.session_list.push(make_entry("eagle", "/p")); // one tab, one leaf
        mgr.active_session = Some("eagle".into());
        mgr.active_tab = Some(0);
        mgr.active_pane = Some("eagle/0".into());
        mgr.swap_focused(1);
        assert!(rx.try_recv().is_err(), "single-pane tab cannot swap");
    }

    #[test]
    fn apply_scheme_sends_apply_layout_scheme() {
        use kmux_protocol::messages::LayoutScheme;
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.active_session = Some("eagle".into());
        mgr.active_tab = Some(0);
        mgr.apply_scheme(LayoutScheme::EvenVertical);
        match rx.try_recv() {
            Ok(ClientMessage::ApplyLayoutScheme {
                word_id,
                tab_index,
                scheme,
            }) => {
                assert_eq!(word_id, "eagle");
                assert_eq!(tab_index, 0);
                assert_eq!(scheme, LayoutScheme::EvenVertical);
            }
            other => panic!("expected ApplyLayoutScheme, got {other:?}"),
        }
    }

    #[test]
    fn cycle_layout_advances_through_presets() {
        use kmux_protocol::messages::LayoutScheme;
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.active_session = Some("eagle".into());
        mgr.active_tab = Some(0);
        // First cycle steps idx 0 → 1, the second preset (EvenVertical).
        mgr.cycle_layout();
        match rx.try_recv() {
            Ok(ClientMessage::ApplyLayoutScheme { scheme, .. }) => {
                assert_eq!(scheme, LayoutScheme::EvenVertical);
            }
            other => panic!("expected ApplyLayoutScheme, got {other:?}"),
        }
    }

    #[test]
    fn render_layout_collapses_to_focused_when_zoomed() {
        use kmux_protocol::messages::{LayoutNode, SplitDir, TabInfo};
        let mut mgr = make_manager();
        let mut entry = make_entry("eagle", "/p");
        entry.tabs = vec![TabInfo {
            tab_index: 0,
            name: "1".into(),
            layout: LayoutNode::Split {
                dir: SplitDir::Horizontal,
                ratios: vec![500, 500],
                children: vec![
                    LayoutNode::Leaf { pane_index: 0 },
                    LayoutNode::Leaf { pane_index: 1 },
                ],
            },
            focused_pane: 1,
        }];
        mgr.session_list.push(entry);
        mgr.active_session = Some("eagle".into());
        mgr.active_tab = Some(0);
        mgr.active_pane = Some("eagle/1".into());
        // Unzoomed: the full tree renders.
        assert!(matches!(
            mgr.render_layout(),
            Some(LayoutNode::Split { .. })
        ));
        // Zoomed: only the focused pane (leaf 1) renders full-area.
        mgr.toggle_zoom();
        assert!(mgr.is_zoomed());
        assert_eq!(mgr.render_layout(), Some(LayoutNode::single(1)));
    }

    #[test]
    fn rename_tab_sends_tab_rename() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.active_session = Some("eagle".into());
        mgr.rename_tab(2, "build");
        match rx.try_recv() {
            Ok(ClientMessage::TabRename {
                word_id,
                tab_index,
                new_name,
                ..
            }) => {
                assert_eq!(word_id, "eagle");
                assert_eq!(tab_index, 2);
                assert_eq!(new_name, "build");
            }
            other => panic!("expected TabRename, got {other:?}"),
        }
    }

    #[test]
    fn tab_renamed_event_updates_cached_name() {
        use kmux_protocol::messages::SessionEventMsg;
        let mut mgr = make_manager();
        mgr.session_list.push(make_entry("eagle", "/p")); // tab 0 starts named "1"
        mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::TabRenamed {
                word_id: "eagle".into(),
                tab_index: 0,
                name: "logs".into(),
            },
        });
        assert_eq!(mgr.session_list[0].tabs[0].name, "logs");
    }

    #[test]
    fn display_name_disambiguation() {
        let mut mgr = make_manager();
        // Two sessions with the same basename "src" but different parent dirs
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "alpha".to_string(),
                name: "src".to_string(),
                cwd: "/proj-a/src".to_string(),
            },
            panes: vec![],
            tabs: vec![],
            active_tab: 0,
            peer: None,
        });
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 1,
                word_id: "beta".to_string(),
                name: "src".to_string(),
                cwd: "/proj-b/src".to_string(),
            },
            panes: vec![],
            tabs: vec![],
            active_tab: 0,
            peer: None,
        });

        assert_eq!(mgr.display_name_for("alpha"), "src (proj-a)");
        assert_eq!(mgr.display_name_for("beta"), "src (proj-b)");
    }

    #[test]
    fn display_name_no_disambiguation_when_unique() {
        let mut mgr = make_manager();
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "eagle".to_string(),
                name: "myapp".to_string(),
                cwd: "/home/user/myapp".to_string(),
            },
            panes: vec![],
            tabs: vec![],
            active_tab: 0,
            peer: None,
        });
        assert_eq!(mgr.display_name_for("eagle"), "myapp");
    }

    #[test]
    fn find_session_by_cwd_returns_matching_word_id() {
        let mut mgr = make_manager();
        mgr.session_list
            .push(make_entry("eagle", "/home/user/proj"));
        mgr.session_list
            .push(make_entry("falcon", "/home/user/other"));

        assert_eq!(
            mgr.find_session_by_cwd("/home/user/proj"),
            Some("eagle".to_string())
        );
        assert_eq!(
            mgr.find_session_by_cwd("/home/user/other"),
            Some("falcon".to_string())
        );
        assert_eq!(mgr.find_session_by_cwd("/nonexistent"), None);
    }

    #[test]
    fn find_session_by_cwd_exact_match_only() {
        let mut mgr = make_manager();
        mgr.session_list
            .push(make_entry("eagle", "/home/user/proj"));

        // Prefix or suffix should not match
        assert_eq!(mgr.find_session_by_cwd("/home/user"), None);
        assert_eq!(mgr.find_session_by_cwd("/home/user/proj/sub"), None);
    }

    #[test]
    fn create_session_with_cwd_sends_correct_message() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.create_session(None, Some("/my/custom/dir"), TermSize::default());

        match rx.try_recv().expect("message sent") {
            ClientMessage::SessionCreate { cwd, .. } => {
                assert_eq!(cwd, Some("/my/custom/dir".to_string()));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn find_session_by_name_matches_display_name() {
        let mut mgr = make_manager();
        mgr.session_list
            .push(make_entry("eagle", "/home/user/proj"));
        // make_entry sets name to basename of cwd
        assert_eq!(mgr.find_session_by_name("proj"), Some("eagle".to_string()));
    }

    #[test]
    fn find_session_by_name_matches_word_id() {
        let mut mgr = make_manager();
        mgr.session_list
            .push(make_entry("eagle", "/home/user/proj"));
        assert_eq!(mgr.find_session_by_name("eagle"), Some("eagle".to_string()));
    }

    #[test]
    fn find_session_by_name_returns_none_for_no_match() {
        let mut mgr = make_manager();
        mgr.session_list
            .push(make_entry("eagle", "/home/user/proj"));
        assert_eq!(mgr.find_session_by_name("nonexistent"), None);
    }

    #[test]
    fn create_session_with_name_and_cwd_sends_correct_message() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.create_session(Some("myapp"), Some("/opt/app"), TermSize::default());

        match rx.try_recv().expect("message sent") {
            ClientMessage::SessionCreate { name, cwd, .. } => {
                assert_eq!(name, Some("myapp".to_string()));
                assert_eq!(cwd, Some("/opt/app".to_string()));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn attach_sends_current_size() {
        // When a pane is attached, the Attach message carries the last stored
        // terminal size rather than the zero default.
        let (mut mgr, mut rx) = make_connected_manager();
        let size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.last_term_size = size;
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.attach_fresh("eagle/0".to_string());

        match rx.try_recv().expect("Attach message sent") {
            ClientMessage::Attach {
                pane_id,
                size: sent_size,
                ..
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(sent_size, size);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn update_term_size_resizes_attached_panes() {
        // update_term_size sends a Resize to every pane tracked in pane_sync.
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.buffers.insert("s1/0".to_string(), CellGrid::default());
        mgr.buffers.insert("s2/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("s1/0".to_string(), PaneSync::AwaitingSync);
        mgr.pane_sync
            .insert("s2/0".to_string(), PaneSync::AwaitingSync);

        let size = TermSize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.update_term_size(size);
        assert_eq!(mgr.last_term_size, size);

        // Both attached panes should receive a Resize.
        let mut resized_panes: Vec<String> = (0..2)
            .map(|_| match rx.try_recv().expect("Resize sent") {
                ClientMessage::Resize { pane_id, size: s } => {
                    assert_eq!(s, size);
                    pane_id
                }
                other => panic!("unexpected: {other:?}"),
            })
            .collect();
        resized_panes.sort();
        assert_eq!(resized_panes, ["s1/0", "s2/0"]);
    }

    #[test]
    fn set_pane_sizes_resizes_synced_pane_on_change_only() {
        // The per-frame layout push (the frontends' `setPaneSizes` / GTK
        // `tiles::push_sizes`) carries the real window size to the daemon after
        // the initial Attach went out at the default 24×80: it must Resize a
        // synced pane the first time its tile size is known and on every change,
        // but never re-send an unchanged size (no PTY thrash).
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync
            .insert("eagle/0".to_string(), PaneSync::AwaitingSync);

        let size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.set_pane_sizes(vec![("eagle/0".to_string(), size)]);
        match rx.try_recv().expect("Resize sent for the synced pane") {
            ClientMessage::Resize { pane_id, size: s } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(s, size);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Same size again: no spurious Resize.
        mgr.set_pane_sizes(vec![("eagle/0".to_string(), size)]);
        assert!(rx.try_recv().is_err(), "unchanged size must not re-send");

        // A new size: another Resize.
        let bigger = TermSize {
            rows: 50,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.set_pane_sizes(vec![("eagle/0".to_string(), bigger)]);
        match rx.try_recv().expect("Resize sent on size change") {
            ClientMessage::Resize { size: s, .. } => assert_eq!(s, bigger),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_pane_sizes_gated_on_attach_but_cached_for_it() {
        // A pane the frontend has laid out but not yet attached must not get a
        // Resize (the daemon has no relay for it yet), but the resolved size is
        // still cached so the subsequent Attach carries it — so the real tile
        // size reaches the daemon regardless of the push/attach ordering.
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());

        let size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.set_pane_sizes(vec![("eagle/0".to_string(), size)]);
        assert!(
            rx.try_recv().is_err(),
            "no Resize before the pane is attached"
        );

        mgr.attach_fresh("eagle/0".to_string());
        match rx.try_recv().expect("Attach sent") {
            ClientMessage::Attach {
                pane_id, size: s, ..
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(s, size, "Attach must carry the cached tile size");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn terminal_update_with_history_gap_issues_fetch_history() {
        // A TerminalUpdate that reports a non-zero `history_total` above what
        // the client has should trigger a single `FetchHistory`. A second
        // update while the first is in flight must not issue another.
        use kmux_protocol::messages::TerminalDiff;
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.buffers
            .insert("eagle/0".to_string(), CellGrid::default());
        mgr.pane_sync.insert(
            "eagle/0".to_string(),
            PaneSync::Synced {
                expected: SequenceNo(0),
            },
        );

        let make_diff = |history_total: u64| {
            Arc::new(TerminalDiff {
                ops: vec![],
                cursor: Default::default(),
                modes: TermModes::EMPTY,
                history_total,
                scrollback_reset: None,
            })
        };

        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff: make_diff(10),
            seqno: SequenceNo(0),
            sent_at_ms: 0,
        });

        // Exactly one FetchHistory on the wire.
        match rx.try_recv().expect("FetchHistory sent") {
            ClientMessage::FetchHistory {
                pane_id,
                start_index,
                count,
                ..
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(start_index, 0);
                assert_eq!(count, 10);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Second diff while still waiting: must not re-issue.
        mgr.handle_server_message(ServerMessage::TerminalUpdate {
            pane_id: "eagle/0".to_string(),
            diff: make_diff(20),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        });
        assert!(rx.try_recv().is_err(), "coalesced: no second FetchHistory");

        // Reply clears the in-flight marker; remaining gap prompts a fresh request.
        let request_id = *mgr
            .in_flight_history_fetches
            .get("eagle/0")
            .expect("in-flight recorded");
        let lines: Vec<Vec<kmux_protocol::messages::CellState>> =
            (0..10).map(|_| Vec::new()).collect();
        mgr.handle_server_message(ServerMessage::HistoryLines {
            request_id,
            pane_id: "eagle/0".to_string(),
            first_index: 0,
            lines,
            history_total: 20,
            sent_at_ms: 0,
        });

        match rx.try_recv().expect("follow-up FetchHistory sent") {
            ClientMessage::FetchHistory {
                start_index, count, ..
            } => {
                assert_eq!(start_index, 10);
                assert_eq!(count, 10);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pane_resized_event_resizes_cellgrid() {
        use kmux_protocol::messages::SessionEventMsg;
        // PaneResized event must resize the local CellGrid buffer so it matches
        // the daemon's new effective size before the forced TerminalSnapshot arrives.
        let (mut mgr, _rx) = make_connected_manager();
        let mut grid = CellGrid::default();
        grid.resize(24, 80);
        mgr.buffers.insert("eagle/0".to_string(), grid);

        let new_size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        };
        mgr.handle_server_message(ServerMessage::Event {
            event: SessionEventMsg::PaneResized {
                pane_id: "eagle/0".to_string(),
                size: new_size,
            },
        });

        let grid = mgr.buffers.get("eagle/0").expect("buffer exists");
        assert_eq!(grid.rows, 40);
        assert_eq!(grid.cols, 120);
    }

    #[test]
    fn request_list_directory_sends_message_and_records_request() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.request_list_directory("/home/user".to_string());

        match rx.try_recv().expect("ListDirectory sent") {
            ClientMessage::ListDirectory { request_id, path } => {
                assert_eq!(path, "/home/user");
                assert_eq!(mgr.pending_dir_request, Some(request_id));
            }
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[test]
    fn directory_listing_stores_and_emits_event_for_matching_request() {
        use super::server_handler::SessionEvent;
        use kmux_protocol::messages::DirEntry;

        let (mut mgr, mut rx) = make_connected_manager();
        mgr.request_list_directory("/home/user".to_string());
        let request_id = match rx.try_recv().unwrap() {
            ClientMessage::ListDirectory { request_id, .. } => request_id,
            other => panic!("unexpected: {other:?}"),
        };

        let events = mgr.handle_server_message(ServerMessage::DirectoryListing {
            request_id,
            path: "/home/user".to_string(),
            parent: Some("/home".to_string()),
            entries: vec![DirEntry {
                name: "dev".to_string(),
                is_dir: true,
            }],
            error: None,
        });

        assert!(matches!(events.as_slice(), [SessionEvent::DirectoryListed]));
        let listing = mgr.dir_listing().expect("listing stored");
        assert_eq!(listing.path, "/home/user");
        assert_eq!(listing.parent.as_deref(), Some("/home"));
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "dev");
        // The pending request is cleared once satisfied.
        assert_eq!(mgr.pending_dir_request, None);
    }

    #[test]
    fn directory_listing_drops_stale_response() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.request_list_directory("/a".to_string());
        let _ = rx.try_recv().unwrap();
        // The user navigated again; only the newest request is pending.
        mgr.request_list_directory("/b".to_string());
        let newest = match rx.try_recv().unwrap() {
            ClientMessage::ListDirectory { request_id, .. } => request_id,
            other => panic!("unexpected: {other:?}"),
        };

        // A late reply for the FIRST request (id = newest - 1) is ignored.
        let events = mgr.handle_server_message(ServerMessage::DirectoryListing {
            request_id: newest - 1,
            path: "/a".to_string(),
            parent: Some("/".to_string()),
            entries: vec![],
            error: None,
        });
        assert!(events.is_empty(), "stale reply must be dropped");
        assert!(
            mgr.dir_listing().is_none(),
            "stale reply must not be stored"
        );
        assert_eq!(mgr.pending_dir_request, Some(newest));
    }

    #[test]
    fn set_paused_sends_setpaused_and_resume_reattaches_visible_panes() {
        let (mut mgr, mut rx) = make_connected_manager();
        mgr.visible_panes = vec!["eagle/0".to_string(), "eagle/1".to_string()];
        mgr.pane_sync.insert(
            "eagle/0".to_string(),
            PaneSync::Synced {
                expected: SequenceNo(5),
            },
        );

        // Pause: a single SetPaused { paused: true }, no re-attach.
        mgr.set_paused(true);
        match rx.try_recv() {
            Ok(ClientMessage::SetPaused { paused: true }) => {}
            other => panic!("expected SetPaused {{ paused: true }}, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "pause must not detach or re-attach");

        // Resume: SetPaused { paused: false } then a full-snapshot re-attach of
        // every visible pane (last_seqno: None → daemon sends final state).
        mgr.set_paused(false);
        let msgs: Vec<ClientMessage> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            matches!(
                msgs.first(),
                Some(ClientMessage::SetPaused { paused: false })
            ),
            "resume must first clear the pause flag, got {msgs:?}"
        );
        let reattached: Vec<&str> = msgs
            .iter()
            .filter_map(|m| match m {
                ClientMessage::Attach {
                    pane_id,
                    last_seqno: None,
                    ..
                } => Some(pane_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reattached,
            vec!["eagle/0", "eagle/1"],
            "resume re-attaches every visible pane with a fresh snapshot"
        );
    }
}
