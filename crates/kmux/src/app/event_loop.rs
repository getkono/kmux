use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use kmux_client::connection_state::DisconnectReason;
use kmux_client::session_manager::SessionEvent;
use kmux_client::ssh;
use kmux_client::supervisor::UpgradeSignal;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::ServerMessage;
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::mode::Mode;
use crate::recent_servers::ServerKind;
use crate::ui;

use super::{App, KeyResult, SwitchTarget};

/// How often to re-check liveness (ping cadence + timeout evaluation).
const LIVENESS_TICK: Duration = Duration::from_secs(1);
/// How often to append one metrics sample to the rolling JSONL file.
/// Must match the cadence documented in `docs/metrics.md`.
const METRICS_FLUSH_TICK: Duration = Duration::from_secs(10);

impl App {
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        let mut event_stream = EventStream::new();
        let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        // Receives a better transport when the background supervisor probe succeeds.
        let (upgrade_tx, mut upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
        // SSH tunnel health: signals when the SSH tunnel process exits unexpectedly.
        let (tunnel_died_tx, mut tunnel_died_rx) = mpsc::channel::<()>(1);
        // When in SSH mode, this sender is used to deliver the ConnectionId to the
        // QUIC upgrade probe task once auth completes.
        let mut quic_probe_conn_id_tx: Option<
            tokio::sync::oneshot::Sender<kmux_protocol::messages::ConnectionId>,
        > = None;

        if let Some(ssh) = self.ssh_session.take() {
            // SSH mode: connect via TCP tunnel, then spawn background QUIC upgrade probe.
            self.mgr
                .set_status_msg("Connecting via SSH tunnel...".to_string());
            quic_probe_conn_id_tx = self
                .connect_via_ssh_session(
                    ssh,
                    srv_tx.clone(),
                    upgrade_tx.clone(),
                    tunnel_died_tx.clone(),
                    None,
                )
                .await;
        } else if !self.connect_token.is_empty() {
            // Normal QUIC mode.
            self.mgr.set_status_msg("Connecting...".to_string());
            self.mgr.connect(srv_tx.clone()).await;
        }

        let render_interval = Duration::from_millis(33); // ~30 FPS
        let mut render_tick = tokio::time::interval(render_interval);
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut liveness_tick = tokio::time::interval(LIVENESS_TICK);
        liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut metrics_flush_tick = tokio::time::interval(METRICS_FLUSH_TICK);
        metrics_flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Render if needed
            if self.needs_render {
                terminal.draw(|f| ui::render(f, self))?;
                self.needs_render = false;
            }

            tokio::select! {
                event = event_stream.next() => {
                    match event {
                        Some(Ok(Event::Key(key_event))) => {
                            match self.handle_key(key_event).await {
                                KeyResult::Quit => return Ok(()),
                                KeyResult::Reconnect => {
                                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                                    srv_rx = new_rx;
                                    quic_probe_conn_id_tx = self
                                        .attempt_reconnect(
                                            new_tx,
                                            upgrade_tx.clone(),
                                            tunnel_died_tx.clone(),
                                        )
                                        .await;
                                }
                                KeyResult::SwitchServer(target) => {
                                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                                    srv_rx = new_rx;
                                    self.did_auto_select = false;
                                    self.mgr.disconnect();
                                    match target {
                                        SwitchTarget::Local => {
                                            self.mgr.set_status_msg("Connecting to local daemon…".to_string());
                                            match kmux_client::daemon::ensure_daemon().await {
                                                Ok(status) => {
                                                    self.is_local = true;
                                                    self.ssh_target = None;
                                                    self.server_display = "localhost".to_string();
                                                    self.server_string = String::new();
                                                    self.server_kind = ServerKind::Local;
                                                    self.mgr.set_connection_params(
                                                        "127.0.0.1".to_string(),
                                                        status.port,
                                                        status.token,
                                                    );
                                                    let events = self.mgr.connect(new_tx).await;
                                                    self.handle_session_events(events);
                                                }
                                                Err(e) => {
                                                    self.mgr.set_status_msg(format!("Daemon start failed: {e}"));
                                                }
                                            }
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
                                            self.mgr.set_status_msg("Connecting via SSH…".to_string());
                                            match ssh::negotiate(&target).await {
                                                Ok(new_ssh) => {
                                                    quic_probe_conn_id_tx = self
                                                        .connect_via_ssh_session(
                                                            new_ssh,
                                                            new_tx.clone(),
                                                            upgrade_tx.clone(),
                                                            tunnel_died_tx.clone(),
                                                            None,
                                                        )
                                                        .await;
                                                }
                                                Err(e) => {
                                                    warn!("SSH switch negotiation failed: {e}");
                                                    self.mgr.set_status_msg(format!("SSH negotiation failed: {e}"));
                                                }
                                            }
                                        }
                                        SwitchTarget::Direct { host, port } => {
                                            // For direct connections we don't have the token,
                                            // so pre-fill the Connect form for the user to enter it.
                                            self.connect_host = host.clone();
                                            self.connect_port = port.to_string();
                                            self.connect_token.clear();
                                            self.ssh_target = None;
                                            self.is_local = false;
                                            self.server_display = format!("{}:{}", host, port);
                                            self.server_string = self.server_display.clone();
                                            self.server_kind = ServerKind::Direct { host, port };
                                            self.mode = Mode::Connect {
                                                field: crate::mode::ConnectField::Token,
                                            };
                                        }
                                    }
                                }
                                KeyResult::Continue => {}
                            }
                            self.needs_render = true;
                        }
                        Some(Ok(Event::Mouse(mouse_event))) => {
                            self.handle_mouse(mouse_event);
                            self.needs_render = true;
                        }
                        Some(Ok(Event::Resize(cols, rows))) => {
                            self.handle_resize(rows, cols);
                            self.needs_render = true;
                        }
                        Some(Err(_)) | None => break,
                        _ => {}
                    }
                }
                // Only poll the server channel while we still expect to
                // receive frames. After a drop the channel is closed and
                // `recv()` resolves synchronously on every poll, which would
                // starve `event_stream.next()` and leave the overlay keys
                // unresponsive.
                msg = srv_rx.recv(), if !matches!(self.mode, Mode::Disconnected { .. }) => {
                    match msg {
                        Some(msg) => {
                            // Drain all available messages
                            let mut batch = vec![msg];
                            while let Ok(m) = srv_rx.try_recv() {
                                batch.push(m);
                            }
                            self.mgr.metrics.record_batch(batch.len());
                            for m in batch {
                                let events = self.mgr.handle_server_message(m);
                                // If auth succeeded and we have a QUIC upgrade probe
                                // waiting for the ConnectionId, deliver it now.
                                for ev in &events {
                                    if matches!(ev, SessionEvent::AuthOk)
                                        && let (Some(tx), Some(conn_id)) =
                                            (quic_probe_conn_id_tx.take(), self.mgr.connection_id)
                                    {
                                        let _ = tx.send(conn_id);
                                    }
                                }
                                self.handle_session_events(events);
                            }
                            self.needs_render = true;
                        }
                        None => {
                            if !matches!(self.mode, Mode::Disconnected { .. }) {
                                self.enter_disconnected(DisconnectReason::ServerClosed);
                                self.needs_render = true;
                            }
                        }
                    }
                }
                tunnel_died = tunnel_died_rx.recv() => {
                    if tunnel_died.is_some()
                        && self.mgr.current_transport == TransportKind::TcpTls
                        && !matches!(self.mode, Mode::Disconnected { .. })
                    {
                        info!("SSH tunnel process exited; freezing session");
                        self.enter_disconnected(DisconnectReason::SshTunnelDied);
                        self.needs_render = true;
                    }
                }
                upgrade = upgrade_rx.recv() => {
                    if let Some(signal) = upgrade {
                        // Signal the new channel is ready, then apply the transport swap.
                        signal.sender.send(kmux_protocol::messages::ClientMessage::ChannelReady).ok();
                        self.mgr.apply_transport_upgrade(signal.sender, signal.new_kind);
                        self.needs_render = true;
                    }
                }
                _ = liveness_tick.tick() => {
                    let now = Instant::now();
                    // Send periodic client ping (no-op unless connected + due).
                    self.mgr.maybe_send_client_ping(now);
                    // Declare timeout when the server stops responding.
                    if self.mgr.is_liveness_timed_out(now)
                        && !matches!(self.mode, Mode::Disconnected { .. })
                    {
                        warn!("Liveness timeout; freezing session");
                        self.enter_disconnected(DisconnectReason::PingTimeout);
                        self.needs_render = true;
                    }
                }
                _ = render_tick.tick() => {
                    // Periodic render for animations (cursor blink, HUD updates)
                    self.needs_render = true;
                }
                _ = metrics_flush_tick.tick() => {
                    // Append one delta sample to the rolling JSONL sink.
                    // No-op if persistence is disabled.
                    let conn_id = self.mgr.connection_id;
                    self.mgr.metrics.flush_sample(conn_id);
                }
            }
        }

        Ok(())
    }

    /// Transition to `Mode::Disconnected`, record the reason in the session
    /// manager, and emit a structured tracing event.
    fn enter_disconnected(&mut self, reason: DisconnectReason) {
        let reason_str = reason.to_string();
        tracing::warn!(
            connection_id = self.mgr.connection_id.map(|c| c.0),
            transport = %self.mgr.current_transport,
            reason = %reason_str,
            "connection dropped",
        );
        self.mgr.mark_connection_lost_with(reason);
        self.disconnect_at = Some(Instant::now());
        self.mode = Mode::Disconnected { reason: reason_str };
    }

    /// Attempt to restore the connection using the current target (local,
    /// SSH, or direct). Returns the oneshot sender carrying the
    /// `ConnectionId` to the supervisor when SSH bootstrap set one up,
    /// otherwise `None`. On failure, transitions to `Mode::Disconnected`
    /// with a bootstrap-failed reason.
    async fn attempt_reconnect(
        &mut self,
        new_tx: mpsc::UnboundedSender<ServerMessage>,
        upgrade_tx: mpsc::Sender<UpgradeSignal>,
        tunnel_died_tx: mpsc::Sender<()>,
    ) -> Option<tokio::sync::oneshot::Sender<kmux_protocol::messages::ConnectionId>> {
        info!(
            connection_id = self.mgr.connection_id.map(|c| c.0),
            "reconnect requested",
        );
        // Drop any lingering connection state (ws_sender, buffers) so the
        // fresh handshake starts from a clean slate. `connection_id` is
        // preserved inside SessionManager for server-side session resumption.
        self.mgr.prepare_reconnect();
        // Show the intermediate state immediately so the user knows the
        // keypress registered even before the handshake completes.
        self.mode = Mode::Normal;
        self.needs_render = true;

        // Refresh the target before bootstrap so we don't dial a stale
        // port/token (e.g. a `kmuxd --self-signed` that restarted on a new
        // random port, or an SSH tunnel whose ephemeral local port moved).
        let ssh_target = self.ssh_target.clone();
        if let Some(target) = ssh_target {
            self.mgr.set_status_msg("Reconnecting via SSH…".to_string());
            match ssh::negotiate(&target).await {
                Ok(new_ssh) => {
                    let connection_id = self.mgr.connection_id;
                    let conn_id_tx = self
                        .connect_via_ssh_session(
                            new_ssh,
                            new_tx,
                            upgrade_tx,
                            tunnel_died_tx,
                            connection_id,
                        )
                        .await;
                    if conn_id_tx.is_none() {
                        self.enter_disconnected(DisconnectReason::BootstrapFailed(
                            "SSH TCP tunnel failed".into(),
                        ));
                    }
                    conn_id_tx
                }
                Err(e) => {
                    warn!("SSH re-negotiation failed: {e}");
                    self.enter_disconnected(DisconnectReason::BootstrapFailed(format!("SSH: {e}")));
                    None
                }
            }
        } else if self.is_local {
            // Local daemon may have restarted on a different random port;
            // re-query the control socket for the live port/token before
            // dialling.
            self.mgr
                .set_status_msg("Reconnecting to local daemon…".to_string());
            match kmux_client::daemon::ensure_daemon().await {
                Ok(status) => {
                    self.mgr.set_connection_params(
                        "127.0.0.1".to_string(),
                        status.port,
                        status.token,
                    );
                    info!(port = status.port, "local daemon port refreshed");
                    let events = self.mgr.connect(new_tx).await;
                    self.handle_session_events(events);
                    self.reflect_bootstrap_outcome();
                }
                Err(e) => {
                    self.enter_disconnected(DisconnectReason::BootstrapFailed(format!(
                        "daemon start failed: {e}"
                    )));
                }
            }
            None
        } else {
            self.mgr.set_connection_params(
                self.connect_host.clone(),
                self.connect_port.parse().unwrap_or(8443),
                self.connect_token.clone(),
            );
            self.mgr.set_status_msg("Reconnecting…".to_string());
            let events = self.mgr.connect(new_tx).await;
            self.handle_session_events(events);
            self.reflect_bootstrap_outcome();
            None
        }
    }

    /// After `mgr.connect()` settles, mirror the manager's connection state
    /// into the TUI mode. On failure, show the disconnect overlay again with
    /// the bootstrap error that `mgr.connect` recorded.
    fn reflect_bootstrap_outcome(&mut self) {
        if self.mgr.connection_state().is_live() {
            self.mode = Mode::Normal;
        } else {
            let reason = match self.mgr.connection_state() {
                kmux_client::connection_state::ConnectionState::Disconnected { reason } => {
                    reason.to_string()
                }
                other => format!("bootstrap failed: {}", other.badge_label()),
            };
            self.mode = Mode::Disconnected { reason };
        }
    }
}
