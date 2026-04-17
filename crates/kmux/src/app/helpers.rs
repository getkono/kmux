use kmux_client::connect::ConnectResult;
use kmux_client::session_manager::SessionEvent;
use kmux_client::ssh::SshSession;
use kmux_client::supervisor::{SupervisorParams, TransportSupervisor, UpgradeSignal};
use kmux_client::tcp_connect;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{ConnectionId, ServerMessage, SessionEntry, TermSize};
use kmux_protocol::transport::bootstrap::EndpointAdvert;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::mode::{ConnectField, Mode};
use crate::recent_servers::RecentServer;

use super::App;

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

    /// Connect to kmuxd via an already-negotiated SSH tunnel.
    ///
    /// Calls `tcp_connect::connect_tcp_tls`, and on success:
    /// - sets the active sender and transport kind to TCP+TLS
    /// - spawns a tunnel health monitor task
    /// - spawns a background `TransportSupervisor` upgrade probe
    ///
    /// Returns the oneshot sender used to deliver the `ConnectionId` to the
    /// supervisor once TCP+TLS auth completes, or `None` if the connection failed.
    pub(super) async fn connect_via_ssh_session(
        &mut self,
        ssh: SshSession,
        srv_tx: mpsc::UnboundedSender<ServerMessage>,
        upgrade_tx: mpsc::Sender<UpgradeSignal>,
        tunnel_died_tx: mpsc::Sender<()>,
        connection_id: Option<ConnectionId>,
    ) -> Option<tokio::sync::oneshot::Sender<ConnectionId>> {
        // TOFU key is remote_host:remote_port (not the local tunnel port) so
        // the pin identifies the actual server across different tunnel sessions.
        let tofu_key = format!("{}:{}", ssh.remote_host, ssh.remote_tcp_port);
        let tcp_result = tcp_connect::connect_tcp_tls(
            "127.0.0.1".to_string(),
            ssh.local_tcp_port,
            tofu_key,
            ssh.token.clone(),
            srv_tx.clone(),
            self.mgr.capabilities().clone(),
            connection_id,
            self.mgr.accept_invalid_certs(),
        )
        .await;

        match tcp_result {
            ConnectResult::Connected(sender) => {
                self.mgr.set_ws_sender(sender);
                self.mgr.current_transport = TransportKind::TcpTls;
                info!("Connected via SSH tunnel (TCP+TLS transport)");

                // Spawn tunnel health monitor.
                let mut tunnel_proc = ssh.tunnel_process;
                let monitor_died_tx = tunnel_died_tx.clone();
                tokio::spawn(async move {
                    let _ = tunnel_proc.wait().await;
                    let _ = monitor_died_tx.send(()).await;
                });

                // Spawn TransportSupervisor to probe for QUIC or other upgrades.
                // The supervisor needs the ConnectionId which is only available after
                // AuthResult, so we deliver it via a oneshot channel.
                let quic_host = ssh.remote_host.clone();
                let quic_port = ssh.quic_port;
                let token = ssh.token.clone();
                let capabilities = self.mgr.capabilities().clone();
                let accept_invalid = self.mgr.accept_invalid_certs();
                let (conn_id_tx, conn_id_rx) = tokio::sync::oneshot::channel::<ConnectionId>();
                let (rtt_tx, rtt_rx) = mpsc::unbounded_channel();
                self.mgr.set_rtt_sink(rtt_tx);
                tokio::spawn(async move {
                    let Ok(conn_id) = conn_id_rx.await else {
                        return;
                    };
                    // Build endpoint list: probe direct QUIC on the remote host.
                    let endpoints = vec![EndpointAdvert {
                        kind: TransportKind::Quic,
                        address: format!("{quic_host}:{quic_port}"),
                    }];
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

                Some(conn_id_tx)
            }
            ConnectResult::Failed(e) => {
                warn!("SSH TCP connection failed: {e}");
                self.mgr
                    .set_status_msg(format!("SSH connection failed: {e}"));
                None
            }
        }
    }

    /// Query the current terminal size, accounting for UI chrome (3 rows).
    pub(super) fn current_term_size() -> TermSize {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        TermSize {
            rows: rows.saturating_sub(3),
            cols,
        }
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
}
