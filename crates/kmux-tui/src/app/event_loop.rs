use std::time::{Duration, Instant};

use super::input_coalesce;

use crossterm::event::EventStream;
use futures::StreamExt;
use kmux_client::connection_state::DisconnectReason;
use kmux_client::supervisor::UpgradeSignal;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::ServerMessage;
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::mode::Mode;
use crate::ui;
use kmux_protocol::messages::TermSize;

use super::{App, BootstrapPhase, BootstrapTaskResult, KeyResult};

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

        // Seed the core (and session manager) with the initial terminal size so
        // the first Attach carries the real dimensions rather than 24×80.
        self.set_term_size(Self::current_term_size());

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
                // Refresh the ratatui-typed theme from the core's agnostic
                // palette (the `/theme` command mutates the core copy).
                self.theme = crate::theme::Theme::from(self.core.palette.clone());
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
                                    // AppCore owns the state change (server
                                    // identity, ssh target, disconnect) and
                                    // hands back the target; the run loop owns
                                    // the channel rebuild + bootstrap.
                                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                                    srv_rx = new_rx;
                                    let resolved = self.prepare_switch(&target);
                                    let (bs_tx, bs_rx) = mpsc::unbounded_channel();
                                    bootstrap_rx = Some(bs_rx);
                                    self.start_bootstrap(
                                        resolved,
                                        new_tx,
                                        BootstrapPhase::Initial,
                                        bs_tx,
                                    );
                                    self.needs_render = true;
                                }
                                KeyResult::Continue => {
                                    // needs_render already set by process_input_batch
                                }
                                // Clipboard effects are handled inside
                                // `handle_key` (toolkit-specific I/O) and never
                                // escape to here; arm kept for exhaustiveness.
                                KeyResult::CopyToClipboard(_) | KeyResult::RequestPaste => {}
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
                                // Server-originated effects (OSC 52 clipboard
                                // writes) are applied via the same arboard path
                                // as user-initiated copies.
                                for eff in self.handle_session_events(events) {
                                    self.apply_clipboard_effect(eff);
                                }
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
                        self.set_term_size(size);
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
}
