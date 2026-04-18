use kmux_client::pipeline::{self, BootstrapOutcome, NoopObserver, ResolvedTarget, SshContext};
use kmux_client::session_manager::SessionEvent;
use kmux_client::supervisor::{SupervisorParams, TransportSupervisor, UpgradeSignal};
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{ServerMessage, SessionEntry, TermSize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::mode::ConnectField;
use crate::mode::Mode;
use crate::recent_servers::RecentServer;

use super::App;

#[derive(Debug)]
pub(super) enum BootstrapPhase {
    Initial,
    Reconnect,
}

/// Result sent from the background bootstrap task to the event loop.
pub(super) enum BootstrapTaskResult {
    Success(Box<BootstrapOutcome>),
    Failed(String),
}

impl App {
    /// React to `SessionEvent`s returned from `SessionManager::handle_server_message`.
    pub(super) fn handle_session_events(&mut self, events: Vec<SessionEvent>) {
        for event in events {
            match event {
                SessionEvent::AuthFailed { .. } => {
                    self.mode = Mode::Connect {
                        field: ConnectField::Host,
                    };
                }
                SessionEvent::AuthOk => {
                    if matches!(self.mode, Mode::Connect { .. }) {
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
                _ => {}
            }
        }
    }

    /// Auto-select or create a session based on CLI flags (--session, --cwd, :path).
    pub(super) fn auto_select_session(&mut self) {
        let size = Self::current_term_size();

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
        } else {
            // Remote without --session or path: show directory picker.
            self.dir_picker_buffer = self.initial_cwd.clone();
            self.mode = Mode::DirectoryPicker;
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

    /// Spawn the tunnel-death monitor and `TransportSupervisor` for a
    /// just-completed SSH bootstrap.
    ///
    /// Must be called immediately after `SessionManager::apply_outcome` so
    /// the supervisor sees the correct `ConnectionId` and the tunnel
    /// process is owned by the monitor task (not leaked).
    pub(super) fn launch_ssh_supervisor(
        &mut self,
        ctx: SshContext,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
        upgrade_tx: mpsc::Sender<UpgradeSignal>,
        tunnel_died_tx: mpsc::Sender<()>,
    ) {
        // Tunnel-death monitor: if the SSH `-L -N` subprocess exits we
        // must signal the event loop so it can surface the disconnect.
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

        tokio::spawn(async move {
            let supervisor = TransportSupervisor::new(SupervisorParams {
                endpoints: ctx.endpoints,
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

    /// Subtract UI chrome (3 rows) from raw terminal dimensions.
    ///
    /// The 3 rows are: session bar (1) + status bar (1) + hint bar (1).
    /// This is the single place that knows the chrome height so future
    /// layout changes only need to be made here.
    pub(super) fn compute_pane_size(rows: u16, cols: u16) -> TermSize {
        TermSize {
            rows: rows.saturating_sub(3),
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// Query the current terminal size, accounting for UI chrome.
    pub(super) fn current_term_size() -> TermSize {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::compute_pane_size(rows, cols)
    }

    /// Write a per-connection metadata log on first successful authentication.
    pub(super) fn write_connection_log(&self) {
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
    /// `BootstrapTaskResult` back via `outcome_tx`. The event loop's
    /// `bootstrap_rx` arm handles the outcome so the UI task stays free
    /// during the entire network handshake.
    ///
    /// Dropping `self.cancel_tx` (set here) causes the spawned task to
    /// abort via a oneshot-receiver drop.
    pub(super) fn start_bootstrap(
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
            (ResolvedTarget::Direct { host, port, .. }, BootstrapPhase::Initial) => {
                format!("Connecting to {host}:{port}…")
            }
            (ResolvedTarget::Direct { host, port, .. }, BootstrapPhase::Reconnect) => {
                format!("Reconnecting to {host}:{port}…")
            }
        };

        self.mode = Mode::Connecting { target_display };
        self.needs_render = true;

        // Store a clone of the sender so the event loop's outcome arm can
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

    /// Build the target to bootstrap against from current App state.
    pub(super) fn current_target(&self) -> kmux_client::pipeline::ResolvedTarget {
        use kmux_client::pipeline::ResolvedTarget;
        if let Some(target) = &self.ssh_target {
            return ResolvedTarget::Ssh {
                target: target.clone(),
                accept_invalid_certs: self.mgr.accept_invalid_certs(),
            };
        }
        if self.is_local {
            return ResolvedTarget::LocalDaemon;
        }
        let port = self.connect_port.parse().unwrap_or(8443);
        ResolvedTarget::Direct {
            host: self.connect_host.clone(),
            port,
            token: self.connect_token.clone(),
            accept_invalid_certs: self.mgr.accept_invalid_certs(),
        }
    }
}
