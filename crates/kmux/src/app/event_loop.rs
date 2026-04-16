use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream};
use futures::StreamExt;
use kmux_client::quic_probe;
use kmux_client::session_manager::SessionEvent;
use kmux_client::ssh;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::ServerMessage;
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::recent_servers::ServerKind;
use crate::ui;

use super::helpers;
use super::{App, KeyResult, SwitchTarget};

const MAX_RECONNECT_ATTEMPTS: u32 = 5;

impl App {
    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        let mut event_stream = EventStream::new();
        let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let mut reconnect_timer: Option<tokio::time::Instant> = None;
        // Receives the new QUIC sender when a background upgrade probe succeeds.
        let (upgrade_tx, mut upgrade_rx) = mpsc::channel::<quic_probe::UpgradeReady>(1);
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
                                    // Replace the server channel so messages from the new
                                    // connection reach the event loop (Bug 1 fix).
                                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                                    srv_rx = new_rx;
                                    self.mgr.set_connection_params(
                                        self.connect_host.clone(),
                                        self.connect_port.parse().unwrap_or(8443),
                                        self.connect_token.clone(),
                                    );
                                    self.mgr.set_status_msg("Connecting...".to_string());
                                    let events = self.mgr.connect(new_tx).await;
                                    self.handle_session_events(events);
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
                                            self.mode = crate::mode::Mode::Connect {
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
                msg = srv_rx.recv() => {
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
                                    if matches!(ev, SessionEvent::AuthOk) {
                                        self.reconnect_attempt = 0;
                                        if let (Some(tx), Some(conn_id)) =
                                            (quic_probe_conn_id_tx.take(), self.mgr.connection_id)
                                        {
                                            let _ = tx.send(conn_id);
                                        }
                                    }
                                }
                                self.handle_session_events(events);
                            }
                            self.needs_render = true;
                        }
                        None => {
                            // Channel closed = disconnected
                            if self.mgr.connected {
                                self.mgr.mark_connection_lost();
                                self.disconnect_at = Some(Instant::now());
                                if self.reconnect_attempt >= MAX_RECONNECT_ATTEMPTS {
                                    self.mgr.set_status_msg(format!(
                                        "Connection lost. Gave up after {MAX_RECONNECT_ATTEMPTS} attempts."
                                    ));
                                } else {
                                    let delay = helpers::backoff_delay(self.reconnect_attempt);
                                    self.reconnect_attempt += 1;
                                    reconnect_timer =
                                        Some(tokio::time::Instant::now() + delay);
                                    self.mgr.set_status_msg(format!(
                                        "Connection lost. Reconnecting in {}s… (attempt {}/{})",
                                        delay.as_secs(),
                                        self.reconnect_attempt,
                                        MAX_RECONNECT_ATTEMPTS
                                    ));
                                }
                                self.needs_render = true;
                            }
                        }
                    }
                }
                _ = async {
                    if let Some(when) = reconnect_timer {
                        tokio::time::sleep_until(when).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    reconnect_timer = None;
                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                    srv_rx = new_rx;

                    let ssh_target = self.ssh_target.clone();
                    if let Some(target) = ssh_target {
                        // SSH mode: re-negotiate to get a fresh tunnel + token.
                        self.mgr.set_status_msg("Reconnecting via SSH…".to_string());
                        match ssh::negotiate(&target).await {
                            Ok(new_ssh) => {
                                let connection_id = self.mgr.connection_id;
                                let conn_id_tx = self
                                    .connect_via_ssh_session(
                                        new_ssh,
                                        new_tx.clone(),
                                        upgrade_tx.clone(),
                                        tunnel_died_tx.clone(),
                                        connection_id,
                                    )
                                    .await;
                                if conn_id_tx.is_none() {
                                    // connect_via_ssh_session failed; schedule next retry.
                                    if self.reconnect_attempt < MAX_RECONNECT_ATTEMPTS {
                                        let delay = helpers::backoff_delay(self.reconnect_attempt);
                                        self.reconnect_attempt += 1;
                                        reconnect_timer =
                                            Some(tokio::time::Instant::now() + delay);
                                    }
                                }
                                quic_probe_conn_id_tx = conn_id_tx;
                            }
                            Err(e) => {
                                warn!("SSH re-negotiation failed: {e}");
                                // Try falling back to direct QUIC if we have connection params.
                                self.mgr.set_status_msg(format!(
                                    "SSH failed ({e}); trying direct QUIC…"
                                ));
                                let events = self.mgr.connect(new_tx).await;
                                self.handle_session_events(events);
                            }
                        }
                    } else {
                        // QUIC or local mode: reconnect directly.
                        self.mgr.set_status_msg("Reconnecting...".to_string());
                        let events = self.mgr.connect(new_tx).await;
                        self.handle_session_events(events);
                    }
                    self.needs_render = true;
                }
                tunnel_died = tunnel_died_rx.recv() => {
                    if tunnel_died.is_some() && self.mgr.connected
                        && self.mgr.current_transport == TransportKind::Tcp
                    {
                        info!("SSH tunnel process exited; triggering reconnect");
                        self.mgr.mark_connection_lost();
                        self.disconnect_at = Some(Instant::now());
                        if self.reconnect_attempt < MAX_RECONNECT_ATTEMPTS {
                            let delay = helpers::backoff_delay(self.reconnect_attempt);
                            self.reconnect_attempt += 1;
                            reconnect_timer = Some(tokio::time::Instant::now() + delay);
                            self.mgr.set_status_msg(format!(
                                "SSH tunnel died. Reconnecting in {}s… (attempt {}/{})",
                                delay.as_secs(), self.reconnect_attempt, MAX_RECONNECT_ATTEMPTS
                            ));
                        } else {
                            self.mgr.set_status_msg(
                                "SSH tunnel died. Gave up reconnecting.".to_string()
                            );
                        }
                        self.needs_render = true;
                    }
                }
                upgrade = upgrade_rx.recv() => {
                    if let Some(ready) = upgrade {
                        // Signal the new QUIC channel is ready to become primary.
                        ready.sender.send(kmux_protocol::messages::ClientMessage::ChannelReady).ok();
                        self.mgr.apply_quic_upgrade(ready.sender);
                        self.needs_render = true;
                    }
                }
                _ = render_tick.tick() => {
                    // Periodic render for animations (cursor blink, HUD updates)
                    self.needs_render = true;
                }
            }
        }

        Ok(())
    }
}
