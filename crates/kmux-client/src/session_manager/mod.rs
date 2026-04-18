mod accessors;
mod connection;
mod input;
mod server_handler;
mod session_ops;

pub use server_handler::SessionEvent;

use std::collections::HashMap;
use std::time::Instant;

use kmux_protocol::messages::{
    ClientCapabilities, ClientId, ClientMessage, PaneId, SequenceNo, SessionEntry, TermSize, WordId,
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

/// Shared client-side session management logic used by both the TUI and GUI frontends.
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

    // Two-level session state
    pub session_list: Vec<SessionEntry>,
    /// Currently active session (word_id).
    pub active_session: Option<WordId>,
    /// Currently active pane (pane_id = "{word_id}/{pane_index}").
    pub active_pane: Option<PaneId>,
    /// Terminal buffers keyed by pane_id.
    pub buffers: HashMap<PaneId, CellGrid>,
    pub(super) pane_sync: HashMap<PaneId, PaneSync>,
    pub input_locked: HashMap<PaneId, bool>,
    pub(super) next_request_id: u64,
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
            active_session: None,
            active_pane: None,
            buffers: HashMap::new(),
            pane_sync: HashMap::new(),
            input_locked: HashMap::new(),
            next_request_id: 0,
            client_id: None,
            metrics: MetricsStore::in_memory(),
            server_version: None,
            connection_id: None,
            current_transport: TransportKind::Quic,
            last_term_size: TermSize::default(),
            connection_state: ConnectionState::Idle,
            liveness: Liveness::new(Instant::now()),
            rtt_tx: None,
        }
    }

    /// Wire the session manager to a `TransportSupervisor`'s RTT sink.
    /// Called by the caller that spawns the supervisor so RTT observations
    /// feed the scorer's EWMA.
    pub fn set_rtt_sink(&mut self, tx: mpsc::UnboundedSender<RttSample>) {
        self.rtt_tx = Some(tx);
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
        if let Err(e) = tx.send(msg) {
            warn!("send_ws failed: {e}");
            return;
        }
        if bytes > 0 {
            self.metrics.record_outbound(bytes);
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

    pub(super) fn attach_fresh(&mut self, pane_id: String) {
        self.pane_sync
            .insert(pane_id.clone(), PaneSync::AwaitingSync);
        self.send_ws(ClientMessage::Attach {
            pane_id,
            last_seqno: None,
            size: self.last_term_size,
        });
    }

    /// Update the stored terminal size and send a `Resize` to every attached pane.
    pub fn update_term_size(&mut self, size: TermSize) {
        self.last_term_size = size;
        let attached: Vec<String> = self
            .buffers
            .keys()
            .filter(|pid| self.pane_sync.contains_key(*pid))
            .cloned()
            .collect();
        for pane_id in attached {
            self.send_ws(ClientMessage::Resize { pane_id, size });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
                pane_id: format!("{word_id}/0"),
                pane_index: 0,
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status: SessionStatus::Running,
            }],
        }
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
            scrollback_lines: vec![],
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
            mgr.buffers.insert(format!("{wid}/0"), CellGrid::default());
            mgr.session_list.push(entry);
        }
        mgr.active_session = Some("c".to_string());
        mgr.active_pane = Some("c/0".to_string());
        mgr.cycle_session(1);
        assert_eq!(mgr.active_session.as_deref(), Some("a")); // wraps from c to a
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
        });
        mgr.session_list.push(SessionEntry {
            meta: SessionMeta {
                index: 1,
                word_id: "beta".to_string(),
                name: "src".to_string(),
                cwd: "/proj-b/src".to_string(),
            },
            panes: vec![],
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
}
