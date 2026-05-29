use std::time::{Duration, Instant};

use super::input_coalesce;

use crossterm::event::EventStream;
use futures::StreamExt;
use kmux_client::connection_state::DisconnectReason;
use kmux_client::pipeline::ResolvedTarget;
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
use kmux_protocol::messages::TermSize;

use super::helpers::{BootstrapPhase, BootstrapTaskResult};
use super::{App, KeyResult, SwitchTarget};

/// How often to re-check liveness (ping cadence + timeout evaluation).
const LIVENESS_TICK: Duration = Duration::from_secs(1);
/// How often to append one metrics sample to the rolling JSONL file.
/// Must match the cadence documented in `docs/metrics.md`.
const METRICS_FLUSH_TICK: Duration = Duration::from_secs(10);
/// Minimum interval between redraws. Caps effective render rate at ~60 FPS and
/// prevents individual input events from each blocking behind a full `terminal.draw`.
const RENDER_MIN_INTERVAL: Duration = Duration::from_millis(16);
/// Debounce window for terminal resize events; must match `event_batch::RESIZE_DEBOUNCE_MS`.
/// SIGWINCH fires continuously during a window drag; coalescing into one resize after the
/// burst settles avoids flooding the server with snapshot fan-outs.
pub(super) const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

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
        // Clipboard reads run on spawn_blocking; paste text arrives here.
        let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<String>();
        self.paste_tx = Some(paste_tx);

        // Seed the session manager with the initial terminal size so the first
        // Attach carries the real dimensions rather than the default 24×80.
        self.mgr.update_term_size(Self::current_term_size());

        // Bootstrap outcome channel — replaced whenever a new bootstrap is kicked off.
        // `None` pends forever so the select! arm is dormant when no bootstrap is running.
        let mut bootstrap_rx: Option<mpsc::UnboundedReceiver<BootstrapTaskResult>> = None;

        // Initial bootstrap: every actionable target (LocalDaemon / Ssh) goes
        // straight to the pipeline. No interactive form gates this anymore.
        if let Some(target) = self.pending_target.take() {
            let (bs_tx, bs_rx) = mpsc::unbounded_channel();
            bootstrap_rx = Some(bs_rx);
            self.start_bootstrap(target, srv_tx.clone(), BootstrapPhase::Initial, bs_tx);
        }

        let render_interval = Duration::from_millis(33); // ~30 FPS
        let mut render_tick = tokio::time::interval(render_interval);
        render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut liveness_tick = tokio::time::interval(LIVENESS_TICK);
        liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut metrics_flush_tick = tokio::time::interval(METRICS_FLUSH_TICK);
        metrics_flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Debounce state: last resize seen, deadline after which it is applied.
        let mut pending_resize: Option<TermSize> = None;
        let mut resize_deadline: Option<tokio::time::Instant> = None;

        // Allow the first frame to draw immediately.
        let mut last_draw = Instant::now()
            .checked_sub(RENDER_MIN_INTERVAL)
            .unwrap_or_else(Instant::now);

        loop {
            if self.force_clear {
                terminal.clear()?;
                self.force_clear = false;
                self.needs_render = true;
            }
            if self.needs_render && last_draw.elapsed() >= RENDER_MIN_INTERVAL {
                terminal.draw(|f| ui::render(f, self))?;
                self.needs_render = false;
                last_draw = Instant::now();
            }

            tokio::select! {
                event = event_stream.next() => {
                    match event {
                        Some(Ok(first_event)) => {
                            let batch = input_coalesce::drain_events(
                                &mut event_stream,
                                first_event,
                            );
                            let result = self
                                .process_input_batch(
                                    batch,
                                    &mut pending_resize,
                                    &mut resize_deadline,
                                )
                                .await;
                            match result {
                                KeyResult::Quit => return Ok(()),
                                KeyResult::Reconnect => {
                                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                                    srv_rx = new_rx;
                                    let (bs_tx, bs_rx) = mpsc::unbounded_channel();
                                    bootstrap_rx = Some(bs_rx);
                                    let target = self.current_target();
                                    self.start_bootstrap(
                                        target,
                                        new_tx,
                                        BootstrapPhase::Reconnect,
                                        bs_tx,
                                    );
                                    self.needs_render = true;
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
                                            let (bs_tx, bs_rx) = mpsc::unbounded_channel();
                                            bootstrap_rx = Some(bs_rx);
                                            self.start_bootstrap(
                                                ResolvedTarget::LocalDaemon,
                                                new_tx,
                                                BootstrapPhase::Initial,
                                                bs_tx,
                                            );
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
                                            let accept_invalid_certs =
                                                self.mgr.accept_invalid_certs();
                                            let (bs_tx, bs_rx) = mpsc::unbounded_channel();
                                            bootstrap_rx = Some(bs_rx);
                                            self.start_bootstrap(
                                                ResolvedTarget::Ssh {
                                                    target,
                                                    accept_invalid_certs,
                                                },
                                                new_tx,
                                                BootstrapPhase::Initial,
                                                bs_tx,
                                            );
                                        }
                                    }
                                    self.needs_render = true;
                                }
                                KeyResult::Continue => {
                                    // needs_render already set by process_input_batch
                                }
                            }
                        }
                        Some(Err(_)) | None => break,
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
                            // In Connecting mode the bootstrap task handles failures;
                            // don't double-transition to Disconnected here.
                            if !matches!(self.mode, Mode::Disconnected { .. } | Mode::Connecting { .. }) {
                                self.enter_disconnected(DisconnectReason::ServerClosed);
                                self.needs_render = true;
                            }
                        }
                    }
                }
                bootstrap_result = async {
                    match bootstrap_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match bootstrap_result {
                        Some(BootstrapTaskResult::Success(outcome)) => {
                            self.cancel_tx = None;
                            // A later success clears any stashed bootstrap error so we
                            // don't re-print a stale failure when the user finally quits.
                            self.last_exit_error = None;
                            let ssh_ctx = self.mgr.apply_outcome(*outcome);
                            if let Some(ctx) = ssh_ctx {
                                let srv_tx_clone = self.pending_srv_tx.take()
                                    .expect("pending_srv_tx set in start_bootstrap");
                                self.launch_ssh_supervisor(
                                    ctx,
                                    srv_tx_clone,
                                    upgrade_tx.clone(),
                                    tunnel_died_tx.clone(),
                                );
                            } else {
                                self.pending_srv_tx = None;
                            }
                            self.reflect_bootstrap_outcome();
                            bootstrap_rx = None;
                            self.needs_render = true;
                        }
                        Some(BootstrapTaskResult::Failed(reason)) => {
                            self.cancel_tx = None;
                            self.pending_srv_tx = None;
                            bootstrap_rx = None;
                            // Stash the multi-line error so it survives terminal teardown
                            // and is re-printed to stderr after the TUI exits. The TUI
                            // disconnect overlay still shows the same text, so an
                            // interactive user can also read it without leaving.
                            self.last_exit_error = Some(reason.clone());
                            self.enter_disconnected(DisconnectReason::BootstrapFailed(reason));
                            self.needs_render = true;
                        }
                        None => {
                            // Bootstrap was cancelled (cancel_tx dropped) or unexpected close.
                            self.cancel_tx = None;
                            self.pending_srv_tx = None;
                            bootstrap_rx = None;
                            self.enter_disconnected(DisconnectReason::BootstrapFailed(
                                "cancelled".to_string(),
                            ));
                            self.needs_render = true;
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
                paste_text = paste_rx.recv() => {
                    if let Some(text) = paste_text {
                        self.mgr.send_paste(text);
                        self.needs_render = true;
                    }
                }
                _ = liveness_tick.tick() => {
                    let now = Instant::now();
                    self.mgr.maybe_send_client_ping(now);
                    if self.mgr.is_liveness_timed_out(now)
                        && !matches!(
                            self.mode,
                            Mode::Disconnected { .. } | Mode::Connecting { .. }
                        )
                    {
                        warn!("Liveness timeout; freezing session");
                        self.enter_disconnected(DisconnectReason::PingTimeout);
                        self.needs_render = true;
                    }
                }
                _ = render_tick.tick() => {
                    self.needs_render = true;
                }
                _ = async {
                    match resize_deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(size) = pending_resize.take() {
                        self.mgr.update_term_size(size);
                        self.needs_render = true;
                    }
                    resize_deadline = None;
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

    /// After the bootstrap outcome arm settles, mirror the manager's connection state
    /// into the TUI mode. On failure, show the disconnect overlay again with
    /// the bootstrap error that `mgr.connect` recorded.
    ///
    /// Only transitions *out of* `Mode::Connecting`; any other mode (e.g.
    /// `DirectoryPicker` picked while bootstrap was in flight) is preserved so
    /// an async bootstrap settling doesn't clobber user-initiated navigation.
    pub(super) fn reflect_bootstrap_outcome(&mut self) {
        if !matches!(self.mode, Mode::Connecting { .. }) {
            return;
        }
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
