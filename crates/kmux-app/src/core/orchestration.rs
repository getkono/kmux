//! Connection/session orchestration methods on [`AppCore`]: the pure-core parts
//! of driving a connection. These read the cached `self.term_size` rather than
//! querying a terminal directly (frontends report their geometry).

use kmux_client::connection_state::{ConnectionState, DisconnectReason};
#[cfg(feature = "remote")]
use kmux_client::pipeline::SshContext;
use kmux_client::pipeline::{self, BootstrapOutcome, NoopObserver, ResolvedTarget};
use kmux_client::session_manager::SessionEvent;
#[cfg(feature = "remote")]
use kmux_client::supervisor::{SupervisorParams, TransportSupervisor, UpgradeSignal};
#[cfg(feature = "remote")]
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{PeerTarget, ServerMessage, SessionEntry};
#[cfg(feature = "remote")]
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{info, warn};

use base64::Engine;

use super::DirBrowserRow;
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
                SessionEvent::PeerOpened { peer } => {
                    // The remote is now federated through the local daemon (issue
                    // #121). Re-arm auto-select and refresh the list so the
                    // remote's sessions — not the pre-federation local list —
                    // drive the picker.
                    info!(%peer, "federated peer opened");
                    if matches!(self.mode, Mode::Connecting { .. }) {
                        self.mode = Mode::Normal;
                    }
                    self.did_auto_select = false;
                    self.mgr.request_session_list();
                }
                SessionEvent::PeerError { reason } => {
                    // The server the user asked for is unreachable (SSH/auth/
                    // connect failure on the daemon's upstream). Surface it like
                    // a failed connect; reconnect re-runs the local link + OpenPeer.
                    warn!(%reason, "federated peer failed to open");
                    self.enter_disconnected(DisconnectReason::BootstrapFailed(reason));
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
            // Remote, no active sessions: browse the daemon host's filesystem to
            // pick where the first session is created (starts at initial_cwd).
            self.open_directory_browser();
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

    /// The directory browser's rows for the current listing + filter, in render
    /// order:
    ///
    /// 1. [`DirBrowserRow::CreateHere`] for `dir_browser_cwd` (always row 0).
    /// 2. [`DirBrowserRow::Up`] for the listing's parent (only if it has one).
    /// 3. one [`DirBrowserRow::Enter`] per subdirectory whose name contains the
    ///    case-insensitive filter [`dir_picker_buffer`](AppCore::dir_picker_buffer).
    ///
    /// The entries/parent come from [`SessionManager::dir_listing`]; until the
    /// first listing arrives only CreateHere is shown. Any listing `error` is
    /// surfaced separately by [`AppCore::dir_browser_error`]; the rows still
    /// include CreateHere (+ Up when known) so the user can always recover.
    pub fn dir_browser_rows(&self) -> Vec<DirBrowserRow> {
        // Prefer the daemon's *canonical* listed path for "create here" so the
        // session is created in the directory actually resolved (which may
        // differ from the requested `dir_browser_cwd` after canonicalization);
        // before the first listing arrives we only know the requested path.
        let create_cwd = self
            .mgr
            .dir_listing()
            .map(|l| l.path.clone())
            .unwrap_or_else(|| self.dir_browser_cwd.clone());
        let mut rows = vec![DirBrowserRow::CreateHere { cwd: create_cwd }];
        let Some(listing) = self.mgr.dir_listing() else {
            return rows;
        };
        if let Some(parent) = &listing.parent {
            rows.push(DirBrowserRow::Up {
                parent: parent.clone(),
            });
        }
        let lower = self.dir_picker_buffer.to_lowercase();
        for entry in &listing.entries {
            if lower.is_empty() || entry.name.to_lowercase().contains(&lower) {
                rows.push(DirBrowserRow::Enter {
                    path: join_path(&listing.path, &entry.name),
                    name: entry.name.clone(),
                });
            }
        }
        rows
    }

    /// The current directory listing's error message, if the last listing
    /// failed (e.g. permission denied). Frontends surface this to the user.
    pub fn dir_browser_error(&self) -> Option<&str> {
        self.mgr.dir_listing().and_then(|l| l.error.as_deref())
    }

    /// Open the directory browser (the "new session — choose a directory"
    /// overlay): seed the browse directory from the active session's cwd
    /// (falling back to [`initial_cwd`](AppCore::initial_cwd)), clear the
    /// filter, request a listing for that directory, and enter
    /// [`Mode::DirectoryPicker`]. Shared by every entry point (the session
    /// picker's "+ New session" row and the remote-no-sessions auto path).
    pub fn open_directory_browser(&mut self) {
        let cwd = self.active_session_cwd().unwrap_or_else(|| {
            if self.dir_browser_cwd.is_empty() {
                self.initial_cwd.clone()
            } else {
                self.dir_browser_cwd.clone()
            }
        });
        self.dir_browser_cwd = cwd.clone();
        self.dir_picker_buffer.clear();
        self.dir_picker_selected = 0;
        self.mgr.request_list_directory(cwd);
        self.mode = Mode::DirectoryPicker;
    }

    /// Navigate the open directory browser to `path`: make it the browse
    /// directory, clear the filter + selection, and request a fresh listing.
    /// Leaves the mode untouched (the browser stays open and refreshes in
    /// place when the listing arrives).
    pub fn navigate_directory_browser(&mut self, path: String) {
        self.dir_browser_cwd = path.clone();
        self.dir_picker_buffer.clear();
        self.dir_picker_selected = 0;
        self.mgr.request_list_directory(path);
    }

    /// The active session's server-side cwd, if a session is active.
    fn active_session_cwd(&self) -> Option<String> {
        let word_id = self.mgr.active_session()?;
        self.mgr
            .session_list()
            .iter()
            .find(|e| e.meta.word_id == word_id)
            .map(|e| e.meta.cwd.clone())
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
    ///
    /// Only present in a `remote` build: a lean GUI never bootstraps an SSH
    /// data plane (it federates remotes through the local daemon), so there is
    /// no tunnel to supervise.
    #[cfg(feature = "remote")]
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
        // Transport override (issue #69): a live channel for runtime changes,
        // plus the current choice seeded into the supervisor so an override set
        // before this (re)connect is honoured immediately.
        let (override_tx, override_rx) = mpsc::unbounded_channel();
        self.mgr.set_override_sink(override_tx);
        let forced = self.mgr.transport_override();

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
                forced,
                override_rx: Some(override_rx),
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
    /// Always the **local daemon** now (issue #121): the GUI's transport is
    /// always UDS-local, and a remote server is federated through the daemon via
    /// [`desired_peer`](AppCore::desired_peer) + `OpenPeer`, not by the GUI
    /// dialling out. Reconnect therefore re-establishes the local link and
    /// re-issues `OpenPeer` (see [`federate_desired_peer`](Self::federate_desired_peer)).
    pub fn current_target(&self) -> ResolvedTarget {
        ResolvedTarget::LocalDaemon
    }

    /// After a successful local (re)connect, ask the daemon to federate the
    /// desired remote peer, if any. Idempotent on the daemon, so re-issuing
    /// after a reconnect simply re-federates.
    pub fn federate_desired_peer(&mut self) {
        if let Some(target) = self.desired_peer.clone() {
            info!(peer = %target.peer_id(), "federating remote peer via local daemon");
            self.mgr.open_peer(target);
        }
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
        self.mgr.disconnect();
        // Every switch re-bootstraps the **local** daemon (issue #121). A remote
        // server is conveyed via `desired_peer` and federated with `OpenPeer`
        // after the local link is back up.
        match target {
            SwitchTarget::Local => {
                self.did_auto_select = false;
                self.is_local = true;
                self.ssh_target = None;
                self.desired_peer = None;
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
                self.desired_peer = Some(PeerTarget::Ssh {
                    user: target.user.clone(),
                    host: target.host.clone(),
                    ssh_port: target.ssh_port,
                    accept_invalid_certs: self.mgr.accept_invalid_certs(),
                });
                // Suppress auto-select on the pre-federation (local) list; it is
                // re-armed when `PeerOpened` arrives with the remote's sessions.
                self.did_auto_select = true;
                ResolvedTarget::LocalDaemon
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

/// Join a daemon-host directory `base` with a child `name` into a full path,
/// using POSIX `/` separators (the daemon is always a Unix host). Avoids a
/// double slash when `base` is the filesystem root.
fn join_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
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
        core.desired_peer = Some(PeerTarget::Ssh {
            user: Some("u".into()),
            host: "h".into(),
            ssh_port: None,
            accept_invalid_certs: true,
        });
        core.server_display = "u@h".into();
        core.server_string = "u@h".into();
        core.did_auto_select = true;

        let resolved = core.prepare_switch(&SwitchTarget::Local);

        assert!(matches!(resolved, ResolvedTarget::LocalDaemon));
        assert!(core.is_local);
        assert!(core.ssh_target.is_none());
        assert!(
            core.desired_peer.is_none(),
            "switching local drops the peer"
        );
        assert_eq!(core.server_display, "localhost");
        assert!(core.server_string.is_empty());
        assert!(matches!(core.server_kind, ServerKind::Local));
        assert!(!core.did_auto_select, "auto-select must reset on switch");
    }

    #[test]
    fn prepare_switch_ssh_federates_through_local_daemon() {
        let mut core = fixture_core();
        let target = RemoteTarget {
            user: Some("alice".into()),
            host: "example.com".into(),
            ssh_port: Some(2222),
        };

        let resolved = core.prepare_switch(&SwitchTarget::Ssh(target));

        // Issue #121: the GUI always bootstraps the local daemon; the remote is
        // federated via `desired_peer` + `OpenPeer`, not a direct GUI dial-out.
        assert!(
            matches!(resolved, ResolvedTarget::LocalDaemon),
            "switching to a remote server must still bootstrap the local daemon",
        );
        match &core.desired_peer {
            Some(PeerTarget::Ssh {
                user,
                host,
                ssh_port,
                ..
            }) => {
                assert_eq!(host, "example.com");
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(*ssh_port, Some(2222));
            }
            other => panic!("expected desired_peer = Ssh, got {other:?}"),
        }
        // Identity still reflects the remote server (drives the UI + auto-select).
        assert!(!core.is_local);
        assert_eq!(core.server_display, "alice@example.com");
        assert_eq!(core.server_string, "alice@example.com");
        assert!(core.ssh_target.is_some());
        assert!(matches!(core.server_kind, ServerKind::Ssh { .. }));
        // Auto-select is suppressed until the federated session list arrives.
        assert!(
            core.did_auto_select,
            "auto-select must be suppressed until PeerOpened",
        );
    }

    #[test]
    fn current_target_is_always_local_even_with_a_desired_peer() {
        let mut core = fixture_core();
        core.prepare_switch(&SwitchTarget::Ssh(RemoteTarget {
            user: None,
            host: "box".into(),
            ssh_port: None,
        }));
        assert!(core.desired_peer.is_some());
        assert!(
            matches!(core.current_target(), ResolvedTarget::LocalDaemon),
            "the GUI's transport is always UDS-local (issue #121)",
        );
    }

    #[test]
    fn federate_desired_peer_sends_open_peer_to_the_daemon() {
        use kmux_protocol::messages::ClientMessage;
        let mut core = fixture_core();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        core.mgr.set_ws_sender(tx);
        core.desired_peer = Some(PeerTarget::Ssh {
            user: Some("alice".into()),
            host: "box".into(),
            ssh_port: Some(2222),
            accept_invalid_certs: true,
        });

        core.federate_desired_peer();

        let saw_open_peer = std::iter::from_fn(|| rx.try_recv().ok()).any(|msg| {
            matches!(
                msg,
                ClientMessage::OpenPeer {
                    target: PeerTarget::Ssh { ref host, .. },
                    ..
                } if host == "box"
            )
        });
        assert!(saw_open_peer, "federate_desired_peer must send OpenPeer");
    }

    #[test]
    fn federate_desired_peer_is_a_noop_without_a_peer() {
        use kmux_protocol::messages::ClientMessage;
        let mut core = fixture_core();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        core.mgr.set_ws_sender(tx);
        core.desired_peer = None;

        core.federate_desired_peer();

        let sent_open_peer = std::iter::from_fn(|| rx.try_recv().ok())
            .any(|msg| matches!(msg, ClientMessage::OpenPeer { .. }));
        assert!(!sent_open_peer, "a local server must not federate anything");
    }

    #[test]
    fn peer_opened_rearms_auto_select_and_refreshes_list() {
        use kmux_protocol::messages::ClientMessage;
        let mut core = fixture_core();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        core.mgr.set_ws_sender(tx);
        core.did_auto_select = true; // suppressed at launch
        core.mode = Mode::Connecting {
            target_display: "x".into(),
        };

        core.handle_session_events(vec![SessionEvent::PeerOpened {
            peer: "alice@box".into(),
        }]);

        assert!(!core.did_auto_select, "PeerOpened must re-arm auto-select");
        assert!(matches!(core.mode, Mode::Normal));
        let saw_list = std::iter::from_fn(|| rx.try_recv().ok())
            .any(|msg| matches!(msg, ClientMessage::SessionList { .. }));
        assert!(
            saw_list,
            "PeerOpened must refresh the federated session list"
        );
    }

    #[test]
    fn peer_error_surfaces_as_a_disconnect() {
        let mut core = fixture_core();
        core.handle_session_events(vec![SessionEvent::PeerError {
            reason: "no route to host".into(),
        }]);
        match &core.mode {
            Mode::Disconnected { reason } => assert!(reason.contains("no route to host")),
            other => panic!("expected Disconnected, got {other:?}"),
        }
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

    // ── Directory browser ────────────────────────────────────────────────────

    use crate::mode::Action;
    use kmux_protocol::messages::{
        ClientMessage, DirEntry, LayoutNode, PaneInfo, SessionMeta, SessionStatus, TabInfo,
        TermSize,
    };
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A core whose manager has a live sender, so sent `ClientMessage`s can be
    /// asserted on the returned receiver. Drains the initial session-list
    /// request that `set_ws_sender` emits.
    fn connected_core() -> (AppCore, UnboundedReceiver<ClientMessage>) {
        let mut core = fixture_core();
        let (tx, mut rx) = mpsc::unbounded_channel();
        core.mgr.set_ws_sender(tx);
        while rx.try_recv().is_ok() {}
        (core, rx)
    }

    fn entry(word_id: &str, cwd: &str) -> SessionEntry {
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word_id.into(),
                name: word_id.into(),
                cwd: cwd.into(),
            },
            panes: vec![PaneInfo {
                pane_id: format!("{word_id}/0"),
                pane_index: 0,
                program: String::new(),
                size: TermSize::default(),
                attached_clients: vec![],
                status: SessionStatus::Running,
                title: String::new(),
            }],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".into(),
                layout: LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
        }
    }

    /// Drive a `DirectoryListing` into the manager for whatever `ListDirectory`
    /// request is currently pending, so the browser sees a listing.
    fn deliver_listing(core: &mut AppCore, path: &str, parent: Option<&str>, dirs: &[&str]) {
        let request_id = core
            .mgr
            .pending_dir_request_for_test()
            .expect("a ListDirectory request must be pending");
        core.mgr
            .handle_server_message(ServerMessage::DirectoryListing {
                request_id,
                path: path.into(),
                parent: parent.map(Into::into),
                entries: dirs
                    .iter()
                    .map(|n| DirEntry {
                        name: (*n).into(),
                        is_dir: true,
                    })
                    .collect(),
                error: None,
            });
    }

    #[test]
    fn opening_browser_seeds_cwd_from_active_session_and_requests_listing() {
        let (mut core, mut rx) = connected_core();
        core.initial_cwd = "/fallback".into();
        core.mgr
            .session_list
            .push(entry("eagle", "/home/user/proj"));
        core.mgr.select_session("eagle".into());
        while rx.try_recv().is_ok() {}

        core.open_directory_browser();

        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_browser_cwd, "/home/user/proj");
        assert!(core.dir_picker_buffer.is_empty());
        match rx.try_recv().expect("a listing was requested") {
            ClientMessage::ListDirectory { path, .. } => assert_eq!(path, "/home/user/proj"),
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[test]
    fn opening_browser_falls_back_to_initial_cwd_without_active_session() {
        let (mut core, mut rx) = connected_core();
        core.initial_cwd = "/fallback".into();

        core.open_directory_browser();

        assert_eq!(core.dir_browser_cwd, "/fallback");
        match rx.try_recv().expect("a listing was requested") {
            ClientMessage::ListDirectory { path, .. } => assert_eq!(path, "/fallback"),
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[test]
    fn dir_browser_rows_order_create_here_then_up_then_filtered_subdirs() {
        let (mut core, _rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(
            &mut core,
            "/home/user",
            Some("/home"),
            &["dev", "docs", "music"],
        );

        let rows = core.dir_browser_rows();
        // CreateHere carries the canonical listed path.
        assert_eq!(
            rows[0],
            DirBrowserRow::CreateHere {
                cwd: "/home/user".into()
            }
        );
        assert_eq!(
            rows[1],
            DirBrowserRow::Up {
                parent: "/home".into()
            }
        );
        assert_eq!(
            &rows[2..],
            &[
                DirBrowserRow::Enter {
                    path: "/home/user/dev".into(),
                    name: "dev".into()
                },
                DirBrowserRow::Enter {
                    path: "/home/user/docs".into(),
                    name: "docs".into()
                },
                DirBrowserRow::Enter {
                    path: "/home/user/music".into(),
                    name: "music".into()
                },
            ]
        );

        // The filter narrows the subdir rows (case-insensitive) and keeps the
        // CreateHere + Up rows so the user can always recover.
        core.dir_picker_buffer = "DO".into();
        let rows = core.dir_browser_rows();
        assert!(matches!(rows[0], DirBrowserRow::CreateHere { .. }));
        assert!(matches!(rows[1], DirBrowserRow::Up { .. }));
        let names: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                DirBrowserRow::Enter { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["docs"]);
    }

    #[test]
    fn dir_browser_rows_omit_up_at_filesystem_root() {
        let (mut core, _rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(&mut core, "/", None, &["bin", "etc"]);

        let rows = core.dir_browser_rows();
        assert!(matches!(rows[0], DirBrowserRow::CreateHere { .. }));
        assert!(
            !rows.iter().any(|r| matches!(r, DirBrowserRow::Up { .. })),
            "no Up row at the filesystem root"
        );
    }

    #[tokio::test]
    async fn submit_create_here_sends_session_create_with_browsed_cwd() {
        let (mut core, mut rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(&mut core, "/srv/app", Some("/srv"), &["sub"]);
        while rx.try_recv().is_ok() {}

        // Row 0 is CreateHere; submit it.
        core.dir_picker_selected = 0;
        core.dispatch_action(Action::DirPickerSubmit).await;

        assert_eq!(core.mode, Mode::Normal);
        match rx.try_recv().expect("a session create was sent") {
            ClientMessage::SessionCreate { cwd, .. } => {
                assert_eq!(cwd.as_deref(), Some("/srv/app"));
            }
            other => panic!("expected SessionCreate, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_subdir_requests_new_listing_and_keeps_browser_open() {
        let (mut core, mut rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(&mut core, "/home/user", Some("/home"), &["dev"]);
        while rx.try_recv().is_ok() {}

        // Select the "dev" Enter row (row 2: CreateHere, Up, dev) and submit.
        core.dir_picker_selected = 2;
        core.dispatch_action(Action::DirPickerSubmit).await;

        // Navigation keeps the browser open and re-targets the browse dir.
        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_browser_cwd, "/home/user/dev");
        match rx.try_recv().expect("a new listing was requested") {
            ClientMessage::ListDirectory { path, .. } => assert_eq!(path, "/home/user/dev"),
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_up_navigates_to_parent() {
        let (mut core, mut rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(&mut core, "/home/user", Some("/home"), &["dev"]);
        while rx.try_recv().is_ok() {}

        core.dir_picker_selected = 1; // the Up row
        core.dispatch_action(Action::DirPickerSubmit).await;

        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_browser_cwd, "/home");
        match rx.try_recv().expect("a parent listing was requested") {
            ClientMessage::ListDirectory { path, .. } => assert_eq!(path, "/home"),
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submit_typed_absolute_path_navigates_when_unmatched() {
        let (mut core, mut rx) = connected_core();
        core.open_directory_browser();
        deliver_listing(&mut core, "/home/user", Some("/home"), &["dev"]);
        while rx.try_recv().is_ok() {}

        // Type an absolute path that matches no listed subdir, then submit while
        // CreateHere (row 0) is selected: the browser navigates to the typed path.
        core.dir_picker_selected = 0;
        core.dir_picker_buffer = "/var/log".into();
        core.dispatch_action(Action::DirPickerSubmit).await;

        assert_eq!(core.mode, Mode::DirectoryPicker);
        assert_eq!(core.dir_browser_cwd, "/var/log");
        match rx
            .try_recv()
            .expect("a listing for the typed path was requested")
        {
            ClientMessage::ListDirectory { path, .. } => assert_eq!(path, "/var/log"),
            other => panic!("expected ListDirectory, got {other:?}"),
        }
    }

    #[test]
    fn dir_browser_surfaces_listing_error() {
        let (mut core, _rx) = connected_core();
        core.open_directory_browser();
        let request_id = core.mgr.pending_dir_request_for_test().unwrap();
        core.mgr
            .handle_server_message(ServerMessage::DirectoryListing {
                request_id,
                path: "/root".into(),
                parent: None,
                entries: vec![],
                error: Some("Permission denied".into()),
            });
        assert_eq!(core.dir_browser_error(), Some("Permission denied"));
        // Even on error, CreateHere is present so the user can recover.
        assert!(matches!(
            core.dir_browser_rows().first(),
            Some(DirBrowserRow::CreateHere { .. })
        ));
    }
}
