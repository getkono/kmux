use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use kmux_client::connection_state::DisconnectReason;
use kmux_client::pipeline::{NoopObserver, ResolvedTarget};
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

use super::helpers::BootstrapPhase;
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

        // Initial bootstrap: if the parsed target is actionable, run the pipeline.
        // Direct-without-token leaves `pending_target` set but enters the Connect
        // form; the user submits that to produce a `Reconnect`.
        if let Some(target) = self.pending_target.take() {
            let actionable = match &target {
                ResolvedTarget::Direct { token, .. } => !token.is_empty(),
                _ => true,
            };
            if actionable {
                self.run_bootstrap(
                    target,
                    srv_tx.clone(),
                    upgrade_tx.clone(),
                    tunnel_died_tx.clone(),
                    BootstrapPhase::Initial,
                )
                .await;
            }
        }

        let render_interval = Duration::from_millis(33); // ~30 FPS
        let mut render_tick = tokio::time::interval(render_interval);
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut liveness_tick = tokio::time::interval(LIVENESS_TICK);
        liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut metrics_flush_tick = tokio::time::interval(METRICS_FLUSH_TICK);
        metrics_flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
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
                                    self.attempt_reconnect(
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
                                            self.is_local = true;
                                            self.ssh_target = None;
                                            self.server_display = "localhost".to_string();
                                            self.server_string = String::new();
                                            self.server_kind = ServerKind::Local;
                                            self.mgr
                                                .set_status_msg("Connecting to local daemon…".to_string());
                                            match self
                                                .mgr
                                                .connect(
                                                    new_tx.clone(),
                                                    ResolvedTarget::LocalDaemon,
                                                    &NoopObserver,
                                                )
                                                .await
                                            {
                                                Ok(_) => self.reflect_bootstrap_outcome(),
                                                Err(e) => self.enter_disconnected(
                                                    DisconnectReason::BootstrapFailed(e.to_string()),
                                                ),
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
                                            let accept_invalid_certs = self.mgr.accept_invalid_certs();
                                            match self
                                                .mgr
                                                .connect(
                                                    new_tx.clone(),
                                                    ResolvedTarget::Ssh {
                                                        target,
                                                        accept_invalid_certs,
                                                    },
                                                    &NoopObserver,
                                                )
                                                .await
                                            {
                                                Ok(Some(ctx)) => {
                                                    self.launch_ssh_supervisor(
                                                        ctx,
                                                        new_tx.clone(),
                                                        upgrade_tx.clone(),
                                                        tunnel_died_tx.clone(),
                                                    );
                                                    self.reflect_bootstrap_outcome();
                                                }
                                                Ok(None) => self.reflect_bootstrap_outcome(),
                                                Err(e) => {
                                                    warn!("SSH switch failed: {e}");
                                                    self.enter_disconnected(
                                                        DisconnectReason::BootstrapFailed(
                                                            e.to_string(),
                                                        ),
                                                    );
                                                }
                                            }
                                        }
                                        SwitchTarget::Direct { host, port } => {
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
                msg = srv_rx.recv(), if !matches!(self.mode, Mode::Disconnected { .. }) => {
                    match msg {
                        Some(msg) => {
                            let mut batch = vec![msg];
                            while let Ok(m) = srv_rx.try_recv() {
                                batch.push(m);
                            }
                            self.mgr.metrics.record_batch(batch.len());
                            for m in batch {
                                let events = self.mgr.handle_server_message(m);
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
                        signal.sender.send(kmux_protocol::messages::ClientMessage::ChannelReady).ok();
                        self.mgr.apply_transport_upgrade(signal.sender, signal.new_kind);
                        self.needs_render = true;
                    }
                }
                _ = liveness_tick.tick() => {
                    let now = Instant::now();
                    self.mgr.maybe_send_client_ping(now);
                    if self.mgr.is_liveness_timed_out(now)
                        && !matches!(self.mode, Mode::Disconnected { .. })
                    {
                        warn!("Liveness timeout; freezing session");
                        self.enter_disconnected(DisconnectReason::PingTimeout);
                        self.needs_render = true;
                    }
                }
                _ = render_tick.tick() => {
                    self.needs_render = true;
                }
                _ = metrics_flush_tick.tick() => {
                    let conn_id = self.mgr.connection_id;
                    self.mgr.metrics.flush_sample(conn_id);
                }
            }
        }

        Ok(())
    }

    /// Transition to `Mode::Disconnected`, record the reason in the session
    /// manager, and emit a structured tracing event.
    pub(super) fn enter_disconnected(&mut self, reason: DisconnectReason) {
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
    /// SSH, or direct). On failure, transitions to `Mode::Disconnected`
    /// with a bootstrap-failed reason.
    async fn attempt_reconnect(
        &mut self,
        new_tx: mpsc::UnboundedSender<ServerMessage>,
        upgrade_tx: mpsc::Sender<UpgradeSignal>,
        tunnel_died_tx: mpsc::Sender<()>,
    ) {
        let target = self.current_target();
        self.run_bootstrap(
            target,
            new_tx,
            upgrade_tx,
            tunnel_died_tx,
            BootstrapPhase::Reconnect,
        )
        .await;
    }

    /// After `mgr.connect()` settles, mirror the manager's connection state
    /// into the TUI mode. On failure, show the disconnect overlay again with
    /// the bootstrap error that `mgr.connect` recorded.
    pub(super) fn reflect_bootstrap_outcome(&mut self) {
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
