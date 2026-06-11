//! Connection/session orchestration methods on [`AppCore`]: the pure-core parts
//! of driving a connection. These read the cached `self.term_size` rather than
//! querying a terminal directly (frontends report their geometry).

use kmux_client::connection_state::{ConnectionState, DisconnectReason};
use kmux_client::pipeline::{self, BootstrapOutcome, NoopObserver, ResolvedTarget, SshContext};
use kmux_client::session_manager::SessionEvent;
use kmux_client::supervisor::{SupervisorParams, TransportSupervisor, UpgradeSignal};
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{ServerMessage, SessionEntry};
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{info, warn};

use base64::Engine;

use crate::mode::Mode;
use crate::recent_servers::{RecentServer, ServerKind};

use super::{AppCore, KeyResult, SwitchTarget};

#[derive(Debug)]
pub enum BootstrapPhase {
    Initial,
    Reconnect,
}

/// Result sent from the background bootstrap task to the frontend's run loop.
pub enum BootstrapTaskResult {
    Success(Box<BootstrapOutcome>),
    Failed(String),
}

/// Map an OSC 52 clipboard-write event to a clipboard effect, applying the
/// active-session policy. Returns `None` when the event is from a pane outside
/// the session the client is currently viewing, or the base64 payload is
/// invalid.
///
/// Last-writer-wins within the active session: a copy from *any* pane in the
/// session you are viewing — not just the focused split — updates the local
/// clipboard, so the most recent OSC 52 write is what a subsequent paste yields.
/// The daemon broadcasts OSC 52 server-wide (see `PaneEventSink` in kmuxd), so
/// scoping to the active session is what keeps a pane in an unrelated background
/// session from clobbering your clipboard. In v1 every selection target is
/// written to the system clipboard; primary-vs-clipboard routing is future work.
fn osc52_clipboard_effect(
    active_session: Option<&str>,
    pane_id: &str,
    _selection: &str,
    data: &str,
) -> Option<KeyResult> {
    // `pane_id` is `"{word_id}/{pane_index}"`; the word_id is the session.
    match (active_session, pane_id.rsplit_once('/')) {
        (Some(session), Some((pane_session, _))) if session == pane_session => {}
        _ => return None,
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    Some(KeyResult::CopyToClipboard(
        String::from_utf8_lossy(&bytes).into_owned(),
    ))
}

impl AppCore {
    /// React to `SessionEvent`s returned from `SessionManager::handle_server_message`.
    ///
    /// Returns the toolkit-specific effects the frontend must perform — currently
    /// only `CopyToClipboard`, from OSC 52 writes honored by the active-session
    /// policy. The frontend applies them with its own clipboard API.
    pub fn handle_session_events(&mut self, events: Vec<SessionEvent>) -> Vec<KeyResult> {
        let mut effects = Vec::new();
        for event in events {
            match event {
                SessionEvent::AuthFailed { .. } => {
                    // SSH-only architecture: auth failure on the data plane
                    // means the SSH tunnel is up but the daemon rejected the
                    // token. Surface as a disconnect; the user can reconnect.
                    self.mode = Mode::Disconnected {
                        reason: "authentication failed".into(),
                    };
                }
                SessionEvent::AuthOk => {
                    if matches!(self.mode, Mode::Connecting { .. }) {
                        self.mode = Mode::Normal;
                    }
                    info!("Auth succeeded");
                    self.write_connection_log();
                    // Record this server as recently used.
                    self.recent_servers.record_connection(
                        &self.server_string.clone(),
                        &self.server_display.clone(),
                        self.server_kind.clone(),
                    );
                }
                SessionEvent::SessionListReceived => {
                    // Update cached session list for current server (self-healing: stale
                    // sessions that no longer exist on the server are silently dropped).
                    let live_sessions = self.mgr.session_list().to_vec();
                    let server_string = self.server_string.clone();
                    self.recent_servers
                        .update_sessions(&server_string, &live_sessions);

                    if !self.did_auto_select {
                        self.did_auto_select = true;
                        self.auto_select_session();
                    }
                }
                SessionEvent::ClipboardCopy {
                    pane_id,
                    selection,
                    data,
                } => {
                    if let Some(eff) = osc52_clipboard_effect(
                        self.mgr.active_session(),
                        &pane_id,
                        &selection,
                        &data,
                    ) {
                        effects.push(eff);
                    }
                }
                _ => {}
            }
        }
        effects
    }

    /// Auto-select or create a session based on CLI flags (--session, --cwd, :path).
    pub fn auto_select_session(&mut self) {
        let size = self.term_size;

        if let Some(session_name) = self.auto_session.take() {
            // --session was given: find by name/word_id or create.
            if let Some(word_id) = self.mgr.find_session_by_name(&session_name) {
                self.mgr.select_session(word_id);
            } else {
                let cwd = self
                    .auto_cwd
                    .take()
                    .unwrap_or_else(|| self.initial_cwd.clone());
                self.mgr
                    .create_session(Some(&session_name), Some(&cwd), size);
            }
        } else if let Some(cwd) = self.auto_cwd.take() {
            // :path or --cwd was given without --session.
            if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                self.mgr.select_session(word_id);
            } else {
                self.mgr.create_session(None, Some(&cwd), size);
            }
        } else if self.is_local {
            // Local mode: match by cwd or create.
            let cwd = self.initial_cwd.clone();
            if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                self.mgr.select_session(word_id);
            } else {
                self.mgr.create_session(None, Some(&cwd), size);
            }
        } else if self.mgr.session_list().is_empty() {
            // Remote, no active sessions: pick a path for the first session.
            self.dir_picker_buffer = self.initial_cwd.clone();
            self.mode = Mode::DirectoryPicker;
        } else {
            // Remote with active sessions: let the user pick one (or hit the
            // synthetic "[+] New session" entry to open the directory picker).
            // Start with the first real session highlighted, not the new-session
            // affordance, so the common case (resume an existing session) is
            // one Enter away.
            self.session_picker_selected = 1;
            self.session_picker_search.clear();
            self.mode = Mode::SessionPicker;
        }
    }

    /// Returns recent servers filtered by the current `server_picker_search` text.
    pub fn filtered_servers(&self) -> Vec<RecentServer> {
        let lower = self.server_picker_search.to_lowercase();
        self.recent_servers
            .servers()
            .iter()
            .filter(|s| {
                lower.is_empty()
                    || s.display.to_lowercase().contains(&lower)
                    || s.server_string.to_lowercase().contains(&lower)
            })
            .cloned()
            .collect()
    }

    /// Returns sessions whose CWD contains the current `dir_picker_buffer` text (case-insensitive).
    pub fn dir_picker_matches(&self) -> Vec<&SessionEntry> {
        let lower = self.dir_picker_buffer.to_lowercase();
        self.mgr
            .session_list()
            .iter()
            .filter(|e| lower.is_empty() || e.meta.cwd.to_lowercase().contains(&lower))
            .collect()
    }

    /// Returns sessions matching the current `session_picker_search` text
    /// (case-insensitive on display name or word_id). An empty search matches
    /// every session. Shared by the session-picker overlay, its navigation
    /// bounds, and the Enter/click selection so the filter has one definition.
    pub fn session_picker_matches(&self) -> Vec<&SessionEntry> {
        let lower = self.session_picker_search.to_lowercase();
        self.mgr
            .session_list()
            .iter()
            .filter(|e| {
                lower.is_empty()
                    || e.meta.name.to_lowercase().contains(&lower)
                    || e.meta.word_id.to_lowercase().contains(&lower)
            })
            .collect()
    }

    /// Spawn the tunnel-death monitor and `TransportSupervisor` for a
    /// just-completed SSH bootstrap.
    ///
    /// Must be called immediately after `SessionManager::apply_outcome` so
    /// the supervisor sees the correct `ConnectionId` and the tunnel
    /// process is owned by the monitor task (not leaked).
    pub fn launch_ssh_supervisor(
        &mut self,
        ctx: SshContext,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
        upgrade_tx: mpsc::Sender<UpgradeSignal>,
        tunnel_died_tx: mpsc::Sender<()>,
    ) {
        // Tunnel-death monitor: if the SSH `-L -N` subprocess exits we
        // must signal the run loop so it can surface the disconnect.
        let mut tunnel_proc = ctx.tunnel_process;
        let monitor_tx = tunnel_died_tx;
        tokio::spawn(async move {
            let _ = tunnel_proc.wait().await;
            let _ = monitor_tx.send(()).await;
        });

        let Some(conn_id) = self.mgr.connection_id else {
            // apply_outcome always sets connection_id on success; missing
            // here implies a misordered caller. Skip supervisor rather
            // than panic so the TCP+TLS path keeps working.
            return;
        };
        let token = self.mgr.token().to_string();
        let capabilities = self.mgr.capabilities().clone();
        let accept_invalid = self.mgr.accept_invalid_certs();
        let (rtt_tx, rtt_rx) = mpsc::unbounded_channel();
        self.mgr.set_rtt_sink(rtt_tx);

        // Compose the supervisor's endpoint set: every probable transport
        // including the currently-active one. Without the active TCP+TLS
        // entry the scorer scoreboard would only show the QUIC candidate,
        // which is misleading (the user can't tell whether QUIC's score
        // actually beats TCP+TLS's) and leaves no fallback registered if
        // QUIC ever becomes the active and then dies.
        let mut endpoints = ctx.endpoints;
        let active_address = format!("{}:{}", self.mgr.host(), self.mgr.port());
        if !endpoints
            .iter()
            .any(|e| e.kind == TransportKind::TcpTls && e.address == active_address)
        {
            endpoints.push(EndpointAdvert {
                kind: TransportKind::TcpTls,
                address: active_address,
            });
        }

        tokio::spawn(async move {
            let supervisor = TransportSupervisor::new(SupervisorParams {
                endpoints,
                connection_id: conn_id,
                token,
                capabilities,
                accept_invalid_certs: accept_invalid,
                active_transport: TransportKind::TcpTls,
                is_local: false,
                server_tx: srv_tx,
                upgrade_tx,
                rtt_rx: Some(rtt_rx),
            });
            supervisor.run().await;
        });
    }

    /// Write a per-connection metadata log on first successful authentication.
    pub fn write_connection_log(&self) {
        kmux_client::connection_log::write_connection_log(
            &self.instance_id,
            env!("CARGO_PKG_VERSION"),
            self.mgr.server_version.as_deref(),
            self.mgr.host(),
            self.mgr.port(),
        );
    }

    /// Spawn a background bootstrap task and enter `Mode::Connecting`.
    ///
    /// The bootstrap task calls `pipeline::run_bootstrap` and sends the
    /// `BootstrapTaskResult` back via `outcome_tx`. The run loop's bootstrap
    /// arm handles the outcome so the frontend stays free during the handshake.
    ///
    /// Dropping `self.cancel_tx` (set here) causes the spawned task to
    /// abort via a oneshot-receiver drop.
    pub fn start_bootstrap(
        &mut self,
        target: ResolvedTarget,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
        phase: BootstrapPhase,
        outcome_tx: mpsc::UnboundedSender<BootstrapTaskResult>,
    ) {
        if matches!(phase, BootstrapPhase::Reconnect) {
            info!(
                connection_id = self.mgr.connection_id.map(|c| c.0),
                "reconnect requested",
            );
        }
        self.mgr.prepare_reconnect();

        let target_display = match (&target, &phase) {
            (ResolvedTarget::LocalDaemon, BootstrapPhase::Initial) => {
                "Connecting to local daemon…".to_string()
            }
            (ResolvedTarget::LocalDaemon, BootstrapPhase::Reconnect) => {
                "Reconnecting to local daemon…".to_string()
            }
            (ResolvedTarget::Ssh { target, .. }, BootstrapPhase::Initial) => {
                let h = match &target.user {
                    Some(u) => format!("{u}@{}", target.host),
                    None => target.host.clone(),
                };
                format!("Connecting via SSH to {h}…")
            }
            (ResolvedTarget::Ssh { target, .. }, BootstrapPhase::Reconnect) => {
                let h = match &target.user {
                    Some(u) => format!("{u}@{}", target.host),
                    None => target.host.clone(),
                };
                format!("Reconnecting via SSH to {h}…")
            }
        };

        self.mode = Mode::Connecting { target_display };
        self.needs_render = true;

        // Store a clone of the sender so the run loop's outcome arm can
        // pass it to `launch_ssh_supervisor` for SSH targets.
        self.pending_srv_tx = Some(srv_tx.clone());

        // Cancel any prior in-flight bootstrap by dropping the old sender.
        let _ = self.cancel_tx.take();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        self.cancel_tx = Some(cancel_tx);

        let capabilities = self.mgr.capabilities().clone();
        let connection_id = self.mgr.connection_id;

        tokio::spawn(async move {
            tokio::select! {
                // Sender dropped → oneshot resolves to Err; either way we abort.
                _ = cancel_rx => {}
                result = pipeline::run_bootstrap(
                    target,
                    capabilities,
                    connection_id,
                    srv_tx,
                    &NoopObserver,
                ) => {
                    let task_result = match result {
                        Ok(outcome) => BootstrapTaskResult::Success(Box::new(outcome)),
                        Err(e) => {
                            warn!("bootstrap failed: {e}");
                            BootstrapTaskResult::Failed(e.to_string())
                        }
                    };
                    let _ = outcome_tx.send(task_result);
                }
            }
        });
    }

    /// Build the target to bootstrap against from current state.
    ///
    /// The app is always in one of two states: local daemon (UDS) or a
    /// remote SSH target. There is no direct-transport bootstrap surface.
    pub fn current_target(&self) -> ResolvedTarget {
        if let Some(target) = &self.ssh_target {
            return ResolvedTarget::Ssh {
                target: target.clone(),
                accept_invalid_certs: self.mgr.accept_invalid_certs(),
            };
        }
        ResolvedTarget::LocalDaemon
    }

    /// Apply the state change for switching to a server chosen from the server
    /// picker, returning the [`ResolvedTarget`] the frontend should bootstrap.
    ///
    /// This is the shared half of `KeyResult::SwitchServer` handling — the
    /// server identity, ssh target, auto-select reset, and disconnect of the
    /// old connection. The frontend owns the toolkit-coupled remainder (replace
    /// the server-message channel, then `start_bootstrap` with the returned
    /// target) so every frontend's run loop drives it identically.
    pub fn prepare_switch(&mut self, target: &SwitchTarget) -> ResolvedTarget {
        self.did_auto_select = false;
        self.mgr.disconnect();
        match target {
            SwitchTarget::Local => {
                self.is_local = true;
                self.ssh_target = None;
                self.server_display = "localhost".to_string();
                self.server_string = String::new();
                self.server_kind = ServerKind::Local;
                ResolvedTarget::LocalDaemon
            }
            SwitchTarget::Ssh(target) => {
                let display = match &target.user {
                    Some(u) => format!("{}@{}", u, target.host),
                    None => target.host.clone(),
                };
                self.server_display = display.clone();
                self.server_string = display;
                self.server_kind = ServerKind::Ssh {
                    user: target.user.clone(),
                    host: target.host.clone(),
                    ssh_port: target.ssh_port,
                };
                self.is_local = false;
                self.ssh_target = Some(target.clone());
                ResolvedTarget::Ssh {
                    target: target.clone(),
                    accept_invalid_certs: self.mgr.accept_invalid_certs(),
                }
            }
        }
    }

    /// Transition to `Mode::Disconnected`, record the reason in the session
    /// manager, and emit a structured tracing event.
    pub fn enter_disconnected(&mut self, reason: DisconnectReason) {
        let reason_str = reason.to_string();
        warn!(
            connection_id = self.mgr.connection_id.map(|c| c.0),
            transport = %self.mgr.current_transport,
            reason = %reason_str,
            "connection dropped",
        );
        self.mgr.mark_connection_lost_with(reason);
        self.disconnect_at = Some(Instant::now());
        self.mode = Mode::Disconnected { reason: reason_str };
    }

    /// After the bootstrap outcome arm settles, mirror the manager's connection
    /// state into the interaction mode. On failure, show the disconnect overlay
    /// again with the bootstrap error that `mgr.connect` recorded.
    ///
    /// Only transitions *out of* `Mode::Connecting`; any other mode (e.g.
    /// `DirectoryPicker` picked while bootstrap was in flight) is preserved so
    /// an async bootstrap settling doesn't clobber user-initiated navigation.
    pub fn reflect_bootstrap_outcome(&mut self) {
        if !matches!(self.mode, Mode::Connecting { .. }) {
            return;
        }
        if self.mgr.connection_state().is_live() {
            self.mode = Mode::Normal;
        } else {
            let reason = match self.mgr.connection_state() {
                ConnectionState::Disconnected { reason } => reason.to_string(),
                other => format!("bootstrap failed: {}", other.badge_label()),
            };
            self.mode = Mode::Disconnected { reason };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_client::session_manager::SessionManager;
    use kmux_client::ssh::RemoteTarget;
    use kmux_protocol::messages::ClientCapabilities;

    fn fixture_core() -> AppCore {
        let mgr = SessionManager::new(
            "127.0.0.1".into(),
            0,
            String::new(),
            true,
            ClientCapabilities::default(),
        );
        AppCore::for_test(mgr)
    }

    #[test]
    fn prepare_switch_local_resets_identity_to_localhost() {
        let mut core = fixture_core();
        // Pretend we were connected to a remote server.
        core.is_local = false;
        core.ssh_target = Some(RemoteTarget {
            user: Some("u".into()),
            host: "h".into(),
            ssh_port: None,
        });
        core.server_display = "u@h".into();
        core.server_string = "u@h".into();
        core.did_auto_select = true;

        let resolved = core.prepare_switch(&SwitchTarget::Local);

        assert!(matches!(resolved, ResolvedTarget::LocalDaemon));
        assert!(core.is_local);
        assert!(core.ssh_target.is_none());
        assert_eq!(core.server_display, "localhost");
        assert!(core.server_string.is_empty());
        assert!(matches!(core.server_kind, ServerKind::Local));
        assert!(!core.did_auto_select, "auto-select must reset on switch");
    }

    #[test]
    fn prepare_switch_ssh_sets_identity_and_returns_target() {
        let mut core = fixture_core();
        let target = RemoteTarget {
            user: Some("alice".into()),
            host: "example.com".into(),
            ssh_port: Some(2222),
        };

        let resolved = core.prepare_switch(&SwitchTarget::Ssh(target));

        match resolved {
            ResolvedTarget::Ssh { target: t, .. } => {
                assert_eq!(t.host, "example.com");
                assert_eq!(t.user.as_deref(), Some("alice"));
                assert_eq!(t.ssh_port, Some(2222));
            }
            _ => panic!("expected Ssh target"),
        }
        assert!(!core.is_local);
        assert_eq!(core.server_display, "alice@example.com");
        assert_eq!(core.server_string, "alice@example.com");
        assert!(core.ssh_target.is_some());
        assert!(matches!(core.server_kind, ServerKind::Ssh { .. }));
    }

    #[test]
    fn osc52_from_active_session_decodes_to_clipboard_effect() {
        // "aGVsbG8=" is base64 for "hello". Active session is "eagle".
        let eff = osc52_clipboard_effect(Some("eagle"), "eagle/0", "c", "aGVsbG8=");
        match eff {
            Some(KeyResult::CopyToClipboard(text)) => assert_eq!(text, "hello"),
            _ => panic!("expected CopyToClipboard"),
        }
    }

    #[test]
    fn osc52_from_non_focused_pane_in_active_session_is_honored() {
        // Last-in-wins: a copy from a non-focused split in the session you are
        // viewing still updates the clipboard (the bug this fix addresses — the
        // old active-pane gate silently dropped it).
        let eff = osc52_clipboard_effect(Some("eagle"), "eagle/3", "c", "aGVsbG8=");
        match eff {
            Some(KeyResult::CopyToClipboard(text)) => assert_eq!(text, "hello"),
            _ => panic!("expected CopyToClipboard"),
        }
    }

    #[test]
    fn osc52_from_other_session_is_ignored() {
        // A pane in a session you are NOT viewing cannot clobber the clipboard,
        // since the daemon broadcasts OSC 52 server-wide.
        let eff = osc52_clipboard_effect(Some("eagle"), "falcon/0", "c", "aGVsbG8=");
        assert!(eff.is_none());

        // No active session at all is likewise ignored.
        let eff = osc52_clipboard_effect(None, "eagle/0", "c", "aGVsbG8=");
        assert!(eff.is_none());

        // A malformed pane_id (no `/`) is ignored.
        let eff = osc52_clipboard_effect(Some("eagle"), "eagle", "c", "aGVsbG8=");
        assert!(eff.is_none());
    }

    #[test]
    fn osc52_invalid_base64_is_ignored() {
        let eff = osc52_clipboard_effect(Some("eagle"), "eagle/0", "c", "not valid base64!");
        assert!(eff.is_none());
    }
}
