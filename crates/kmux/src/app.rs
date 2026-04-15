use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, EventStream, KeyEvent, MouseEvent, MouseEventKind};
use futures::StreamExt;
use kmux_client::connect::ConnectResult;
use kmux_client::input::{encode_mouse_scroll, key_to_bytes};
use kmux_client::quic_probe;
use kmux_client::session_manager::{SessionEvent, SessionManager};
use kmux_client::ssh::{self, RemoteTarget, SshSession};
use kmux_client::tcp_connect;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{PROTOCOL_VERSION, ServerMessage, SessionEntry, TermSize};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::key_convert;
use crate::mode::{self, Action, ConnectField, Mode};
use crate::theme::Theme;
use crate::ui;

/// What `handle_key` returns to the event loop.
enum KeyResult {
    Continue,
    Quit,
    /// User submitted the Connect form; the event loop must replace `srv_rx`.
    Reconnect,
}

pub struct App {
    pub mgr: SessionManager,

    // TUI-specific state
    pub theme: Theme,
    pub mode: Mode,
    pub hud_visible: bool,
    pub force_snapshot_mode: bool,

    // Connect form input fields
    pub connect_host: String,
    pub connect_port: String,
    pub connect_token: String,

    // Reconnection bookkeeping
    pub disconnect_at: Option<Instant>,

    // Session picker state
    pub session_picker_selected: usize,
    pub session_picker_search: String,

    // Directory picker state (remote connections)
    pub dir_picker_buffer: String,
    pub dir_picker_selected: usize,

    // Auto-session selection context
    pub is_local: bool,
    pub initial_cwd: String,
    did_auto_select: bool,

    /// CLI `--session` flag: auto-attach to a session by name or word_id.
    auto_session: Option<String>,
    /// Effective cwd from `--cwd` or `:path` in server string.
    auto_cwd: Option<String>,

    /// Width (in columns) of the session badge in the top bar, used to detect
    /// mouse clicks that should open the session picker.
    pub session_badge_cols: u16,

    needs_render: bool,

    /// Unique ID for this client process, written to the connection log on auth success.
    instance_id: String,

    /// Active SSH session (tunnel process + connection metadata) when in SSH mode.
    /// Kept alive as long as the TCP transport is in use; dropped on QUIC upgrade.
    ssh_session: Option<SshSession>,

    /// SSH target stored for re-negotiation when the tunnel dies (SSH mode only).
    ssh_target: Option<RemoteTarget>,

    /// Consecutive reconnect failures. Reset on successful auth. Used for exponential backoff.
    reconnect_attempt: u32,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port: u16,
        token: String,
        accept_invalid_certs: bool,
        is_local: bool,
        initial_cwd: String,
        theme: Theme,
        instance_id: String,
        ssh_session: Option<SshSession>,
        ssh_target: Option<RemoteTarget>,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
    ) -> Self {
        let connect_host = host.clone();
        let connect_port = port.to_string();
        let connect_token = token.clone();

        let initial_mode = if token.is_empty() {
            Mode::Connect {
                field: ConnectField::Host,
            }
        } else {
            Mode::Normal
        };

        let capabilities = crate::host_caps::detect();

        Self {
            mgr: SessionManager::new(host, port, token, accept_invalid_certs, capabilities),
            theme,
            mode: initial_mode,
            hud_visible: false,
            force_snapshot_mode: false,
            connect_host,
            connect_port,
            connect_token,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            is_local,
            initial_cwd,
            did_auto_select: false,
            session_badge_cols: 0,
            needs_render: true,
            instance_id,
            ssh_session,
            ssh_target,
            auto_session,
            auto_cwd,
            reconnect_attempt: 0,
        }
    }

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
            let tcp_result = tcp_connect::connect_tcp(
                "127.0.0.1".to_string(),
                ssh.local_tcp_port,
                ssh.token.clone(),
                srv_tx.clone(),
                self.mgr.capabilities().clone(),
                None,
            )
            .await;
            match tcp_result {
                ConnectResult::Connected(sender) => {
                    self.mgr.set_ws_sender(sender);
                    self.mgr.current_transport = TransportKind::Tcp;
                    info!("Connected via SSH tunnel (TCP transport)");

                    // Split the tunnel process from the session so we can monitor it.
                    let mut tunnel_proc = ssh.tunnel_process;
                    let quic_host = ssh.remote_host.clone();
                    let quic_port = ssh.quic_port;
                    let token = ssh.token.clone();

                    // Spawn tunnel health monitor — keeps process alive and signals on exit.
                    let monitor_died_tx = tunnel_died_tx.clone();
                    tokio::spawn(async move {
                        let _ = tunnel_proc.wait().await;
                        let _ = monitor_died_tx.send(()).await;
                    });

                    // Spawn QUIC upgrade probe in the background.
                    // The probe waits for the ConnectionId (sent via a oneshot after auth).
                    let capabilities = self.mgr.capabilities().clone();
                    let accept_invalid = self.mgr.accept_invalid_certs();
                    let probe_srv_tx = srv_tx.clone();
                    let probe_upgrade_tx = upgrade_tx.clone();
                    let (conn_id_tx, conn_id_rx) =
                        tokio::sync::oneshot::channel::<kmux_protocol::messages::ConnectionId>();
                    quic_probe_conn_id_tx = Some(conn_id_tx);
                    tokio::spawn(async move {
                        // Wait for the TCP auth to deliver the ConnectionId.
                        if let Ok(conn_id) = conn_id_rx.await {
                            quic_probe::quic_upgrade_loop(quic_probe::QuicProbeParams {
                                remote_host: quic_host,
                                quic_port,
                                token,
                                connection_id: conn_id,
                                capabilities,
                                accept_invalid_certs: accept_invalid,
                                srv_tx: probe_srv_tx,
                                upgrade_tx: probe_upgrade_tx,
                                max_failures: 10,
                            })
                            .await;
                        }
                    });
                }
                ConnectResult::Failed(e) => {
                    warn!("TCP/SSH connection failed: {e}");
                    self.mgr
                        .set_status_msg(format!("SSH connection failed: {e}"));
                }
            }
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
                                const MAX_ATTEMPTS: u32 = 5;
                                if self.reconnect_attempt >= MAX_ATTEMPTS {
                                    self.mgr.set_status_msg(format!(
                                        "Connection lost. Gave up after {MAX_ATTEMPTS} attempts."
                                    ));
                                } else {
                                    let delay = backoff_delay(self.reconnect_attempt);
                                    self.reconnect_attempt += 1;
                                    reconnect_timer =
                                        Some(tokio::time::Instant::now() + delay);
                                    self.mgr.set_status_msg(format!(
                                        "Connection lost. Reconnecting in {}s… (attempt {}/{})",
                                        delay.as_secs(),
                                        self.reconnect_attempt,
                                        MAX_ATTEMPTS
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

                    if let Some(ref target) = self.ssh_target {
                        // SSH mode: re-negotiate to get a fresh tunnel + token.
                        self.mgr.set_status_msg("Reconnecting via SSH…".to_string());
                        match ssh::negotiate(target).await {
                            Ok(new_ssh) => {
                                let tcp_result = tcp_connect::connect_tcp(
                                    "127.0.0.1".to_string(),
                                    new_ssh.local_tcp_port,
                                    new_ssh.token.clone(),
                                    new_tx.clone(),
                                    self.mgr.capabilities().clone(),
                                    self.mgr.connection_id,
                                )
                                .await;
                                match tcp_result {
                                    ConnectResult::Connected(sender) => {
                                        self.mgr.set_ws_sender(sender);
                                        self.mgr.current_transport = TransportKind::Tcp;
                                        info!("Reconnected via SSH tunnel (TCP transport)");

                                        // Split tunnel process for health monitoring.
                                        let mut tunnel_proc = new_ssh.tunnel_process;
                                        let quic_host = new_ssh.remote_host.clone();
                                        let quic_port = new_ssh.quic_port;
                                        let token = new_ssh.token.clone();

                                        // Spawn new tunnel health monitor.
                                        let monitor_died_tx2 = tunnel_died_tx.clone();
                                        tokio::spawn(async move {
                                            let _ = tunnel_proc.wait().await;
                                            let _ = monitor_died_tx2.send(()).await;
                                        });

                                        // Spawn new QUIC upgrade probe.
                                        let caps = self.mgr.capabilities().clone();
                                        let accept_invalid = self.mgr.accept_invalid_certs();
                                        let probe_srv_tx = new_tx.clone();
                                        let upg_tx = upgrade_tx.clone();
                                        let (conn_id_tx2, conn_id_rx2) =
                                            tokio::sync::oneshot::channel::<
                                                kmux_protocol::messages::ConnectionId,
                                            >();
                                        quic_probe_conn_id_tx = Some(conn_id_tx2);
                                        tokio::spawn(async move {
                                            if let Ok(cid) = conn_id_rx2.await {
                                                quic_probe::quic_upgrade_loop(
                                                    quic_probe::QuicProbeParams {
                                                        remote_host: quic_host,
                                                        quic_port,
                                                        token,
                                                        connection_id: cid,
                                                        capabilities: caps,
                                                        accept_invalid_certs: accept_invalid,
                                                        srv_tx: probe_srv_tx,
                                                        upgrade_tx: upg_tx,
                                                        max_failures: 10,
                                                    },
                                                )
                                                .await;
                                            }
                                        });
                                    }
                                    ConnectResult::Failed(e) => {
                                        warn!("SSH reconnect TCP failed: {e}");
                                        self.mgr.set_status_msg(format!(
                                            "Reconnect failed: {e}"
                                        ));
                                        // Schedule next retry with backoff.
                                        const MAX_ATTEMPTS: u32 = 5;
                                        if self.reconnect_attempt < MAX_ATTEMPTS {
                                            let delay = backoff_delay(self.reconnect_attempt);
                                            self.reconnect_attempt += 1;
                                            reconnect_timer = Some(
                                                tokio::time::Instant::now() + delay,
                                            );
                                        }
                                    }
                                }
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
                        const MAX_ATTEMPTS: u32 = 5;
                        if self.reconnect_attempt < MAX_ATTEMPTS {
                            let delay = backoff_delay(self.reconnect_attempt);
                            self.reconnect_attempt += 1;
                            reconnect_timer = Some(tokio::time::Instant::now() + delay);
                            self.mgr.set_status_msg(format!(
                                "SSH tunnel died. Reconnecting in {}s… (attempt {}/{})",
                                delay.as_secs(), self.reconnect_attempt, MAX_ATTEMPTS
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

    /// React to `SessionEvent`s returned from `SessionManager::handle_server_message`.
    fn handle_session_events(&mut self, events: Vec<SessionEvent>) {
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
                }
                SessionEvent::SessionListReceived => {
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
    fn auto_select_session(&mut self) {
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
                    .create_session_with_name_and_cwd(&session_name, &cwd, size);
            }
        } else if let Some(cwd) = self.auto_cwd.take() {
            // :path or --cwd was given without --session.
            if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                self.mgr.select_session(word_id);
            } else {
                self.mgr.create_session_with_cwd(&cwd, size);
            }
        } else if self.is_local {
            // Local mode: match by cwd or create.
            let cwd = self.initial_cwd.clone();
            if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                self.mgr.select_session(word_id);
            } else {
                self.mgr.create_session_with_cwd(&cwd, size);
            }
        } else {
            // Remote without --session or path: show directory picker.
            self.dir_picker_buffer = self.initial_cwd.clone();
            self.mode = Mode::DirectoryPicker;
        }
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

    /// Handle a key event. Returns the appropriate `KeyResult` for the event loop.
    async fn handle_key(&mut self, key_event: KeyEvent) -> KeyResult {
        let (key, mods) = key_convert::convert(&key_event);
        let (new_mode, action) = mode::resolve(&self.mode, &key, mods);

        if let Some(m) = new_mode {
            self.mode = m;
        }

        match action {
            Action::ForwardKey => {
                // Snap to bottom on keypress
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_to_bottom();
                }

                let app_cursor = self
                    .mgr
                    .active_grid()
                    .map(|b| b.app_cursor())
                    .unwrap_or(false);
                let text = key_convert::text_from_event(&key_event);
                let bytes = key_to_bytes(&key, mods, text.as_deref(), app_cursor);
                if let Some(bytes) = bytes {
                    self.mgr.send_input(bytes);
                }
            }
            Action::CreateSession => {
                self.mgr.create_session(Self::current_term_size());
            }
            Action::CreatePane => {
                self.mgr.create_pane(Self::current_term_size());
            }
            Action::CloseSession => {
                if let Some(word_id) = self.mgr.active_session().map(|s| s.to_string()) {
                    self.mode = Mode::ConfirmCloseSession { word_id };
                }
            }
            Action::ClosePane => {
                self.mgr.close_pane();
            }
            Action::ConfirmCloseYes => {
                if let Mode::ConfirmCloseSession { word_id } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
                {
                    self.mgr.close_session(&word_id);
                }
            }
            Action::NextSession => self.mgr.cycle_session(1),
            Action::PrevSession => self.mgr.cycle_session(-1),
            Action::NextPane => self.mgr.cycle_pane(1),
            Action::PrevPane => self.mgr.cycle_pane(-1),
            Action::JumpToSession(idx) => {
                if idx < self.mgr.session_list().len() {
                    let word_id = self.mgr.session_list()[idx].meta.word_id.clone();
                    self.mgr.select_session(word_id);
                }
            }
            Action::RenameSession => {
                if let Some(word_id) = self.mgr.active_session().map(|s| s.to_string()) {
                    let current_name = self
                        .mgr
                        .session_list()
                        .iter()
                        .find(|e| e.meta.word_id == word_id)
                        .map(|e| e.meta.name.clone())
                        .unwrap_or_default();
                    self.mode = Mode::RenameSession {
                        buffer: current_name,
                        word_id,
                    };
                }
            }
            Action::RenameChar(ch) => {
                if let Mode::RenameSession { buffer, .. } = &mut self.mode {
                    buffer.push(ch);
                }
            }
            Action::RenameBackspace => {
                if let Mode::RenameSession { buffer, .. } = &mut self.mode {
                    buffer.pop();
                }
            }
            Action::RenameSubmit => {
                if let Mode::RenameSession { buffer, word_id } =
                    std::mem::replace(&mut self.mode, Mode::Normal)
                {
                    let new_name = buffer.trim().to_string();
                    self.mgr.rename_session(&word_id, &new_name);
                }
            }
            Action::CloseSessionPicker => {
                self.mode = Mode::Normal;
            }
            Action::SelectPickerEntry => {
                let search = self.session_picker_search.to_lowercase();
                let matches: Vec<_> = self
                    .mgr
                    .session_list()
                    .iter()
                    .filter(|e| {
                        search.is_empty()
                            || e.meta.name.to_lowercase().contains(&search)
                            || e.meta.word_id.to_lowercase().contains(&search)
                    })
                    .map(|e| e.meta.word_id.clone())
                    .collect();
                if let Some(word_id) = matches.get(self.session_picker_selected) {
                    self.mgr.select_session(word_id.clone());
                }
                self.mode = Mode::Normal;
            }
            Action::PickerUp => {
                if self.session_picker_selected > 0 {
                    self.session_picker_selected -= 1;
                }
            }
            Action::PickerDown => {
                let count = self
                    .mgr
                    .session_list()
                    .iter()
                    .filter(|e| {
                        let s = self.session_picker_search.to_lowercase();
                        s.is_empty()
                            || e.meta.name.to_lowercase().contains(&s)
                            || e.meta.word_id.to_lowercase().contains(&s)
                    })
                    .count();
                if count > 0 && self.session_picker_selected + 1 < count {
                    self.session_picker_selected += 1;
                }
            }
            Action::PickerSearchChar(ch) => {
                self.session_picker_search.push(ch);
                self.session_picker_selected = 0;
            }
            Action::PickerSearchBackspace => {
                self.session_picker_search.pop();
                self.session_picker_selected = 0;
            }
            Action::Disconnect => {
                self.mgr.disconnect();
                self.mode = Mode::Connect {
                    field: ConnectField::Host,
                };
            }
            Action::SendSignal(signal) => {
                if let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) {
                    self.mgr.send_signal(&pane_id, signal);
                }
            }
            Action::ScrollUp(n) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_up(n);
                }
            }
            Action::ScrollDown(n) => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    grid.scroll_down(n);
                }
            }
            Action::ScrollPageUp => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    let rows = grid.rows;
                    grid.scroll_up(rows);
                }
            }
            Action::ScrollPageDown => {
                if let Some(grid) = self.mgr.active_grid_mut() {
                    let rows = grid.rows;
                    grid.scroll_down(rows);
                }
            }
            Action::ToggleHud => {
                self.hud_visible = !self.hud_visible;
            }
            Action::ToggleSnapshotMode => {
                self.force_snapshot_mode = !self.force_snapshot_mode;
                self.mgr.set_snapshot_mode(self.force_snapshot_mode);
            }
            Action::ToggleInputLock => {
                self.mgr.toggle_input_lock();
            }
            Action::CopySelection => {
                if let Some(text) = self.mgr.active_grid().and_then(|g| g.selected_text()) {
                    let _ = cli_clipboard::set_contents(text);
                }
            }
            Action::Paste => {
                if let Ok(text) = cli_clipboard::get_contents() {
                    self.mgr.send_paste(text);
                }
            }
            Action::ConnectSubmit => {
                return KeyResult::Reconnect;
            }
            Action::ConnectNextField => {
                self.mode = match &self.mode {
                    Mode::Connect {
                        field: ConnectField::Host,
                    } => Mode::Connect {
                        field: ConnectField::Port,
                    },
                    Mode::Connect {
                        field: ConnectField::Port,
                    } => Mode::Connect {
                        field: ConnectField::Token,
                    },
                    Mode::Connect {
                        field: ConnectField::Token,
                    } => Mode::Connect {
                        field: ConnectField::Host,
                    },
                    other => other.clone(),
                };
            }
            Action::ConnectPrevField => {
                self.mode = match &self.mode {
                    Mode::Connect {
                        field: ConnectField::Host,
                    } => Mode::Connect {
                        field: ConnectField::Token,
                    },
                    Mode::Connect {
                        field: ConnectField::Port,
                    } => Mode::Connect {
                        field: ConnectField::Host,
                    },
                    Mode::Connect {
                        field: ConnectField::Token,
                    } => Mode::Connect {
                        field: ConnectField::Port,
                    },
                    other => other.clone(),
                };
            }
            Action::ConnectChar(ch) => {
                if let Mode::Connect { field } = &self.mode {
                    match field {
                        ConnectField::Host => self.connect_host.push(ch),
                        ConnectField::Port => self.connect_port.push(ch),
                        ConnectField::Token => self.connect_token.push(ch),
                    }
                }
            }
            Action::ConnectBackspace => {
                if let Mode::Connect { field } = &self.mode {
                    match field {
                        ConnectField::Host => {
                            self.connect_host.pop();
                        }
                        ConnectField::Port => {
                            self.connect_port.pop();
                        }
                        ConnectField::Token => {
                            self.connect_token.pop();
                        }
                    }
                }
            }
            Action::ExitToNormal => {
                self.mode = Mode::Normal;
            }
            Action::DirPickerChar(ch) => {
                self.dir_picker_buffer.push(ch);
                self.dir_picker_selected = 0;
            }
            Action::DirPickerBackspace => {
                self.dir_picker_buffer.pop();
                self.dir_picker_selected = 0;
            }
            Action::DirPickerUp => {
                self.dir_picker_selected = self.dir_picker_selected.saturating_sub(1);
            }
            Action::DirPickerDown => {
                let count = self.dir_picker_matches().len();
                if count > 0 && self.dir_picker_selected + 1 < count {
                    self.dir_picker_selected += 1;
                }
            }
            Action::DirPickerSubmit => {
                let matches = self.dir_picker_matches();
                if let Some(entry) = matches.get(self.dir_picker_selected) {
                    let word_id = entry.meta.word_id.clone();
                    self.mgr.select_session(word_id);
                } else {
                    let cwd = self.dir_picker_buffer.trim().to_string();
                    if !cwd.is_empty() {
                        if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                            self.mgr.select_session(word_id);
                        } else {
                            self.mgr
                                .create_session_with_cwd(&cwd, Self::current_term_size());
                        }
                    }
                }
            }
            Action::DirPickerCancel => {}
            Action::Quit => {
                return KeyResult::Quit;
            }
            Action::None => {}
        }

        KeyResult::Continue
    }

    fn handle_mouse(&mut self, event: MouseEvent) {
        // Click on the session badge row opens the session picker
        if event.row == 0
            && event.column < self.session_badge_cols
            && matches!(event.kind, MouseEventKind::Down(_))
        {
            self.session_picker_selected = 0;
            self.session_picker_search.clear();
            self.mode = Mode::SessionPicker;
            return;
        }

        let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) else {
            return;
        };
        match event.kind {
            MouseEventKind::ScrollUp => {
                let use_pty = self
                    .mgr
                    .buffer(&pane_id)
                    .map(|g| g.modes().mouse_report())
                    .unwrap_or(false);
                if use_pty {
                    let col = event.column + 1;
                    let row = event.row + 1;
                    let sgr = self
                        .mgr
                        .buffer(&pane_id)
                        .map(|g| g.modes().sgr_mouse())
                        .unwrap_or(false);
                    let bytes = encode_mouse_scroll(col, row, 3, sgr);
                    if !bytes.is_empty() {
                        self.mgr.send_input(bytes);
                    }
                } else if let Some(grid) = self.mgr.buffer_mut(&pane_id) {
                    grid.scroll_up(3);
                }
            }
            MouseEventKind::ScrollDown => {
                let use_pty = self
                    .mgr
                    .buffer(&pane_id)
                    .map(|g| g.modes().mouse_report())
                    .unwrap_or(false);
                if use_pty {
                    let col = event.column + 1;
                    let row = event.row + 1;
                    let sgr = self
                        .mgr
                        .buffer(&pane_id)
                        .map(|g| g.modes().sgr_mouse())
                        .unwrap_or(false);
                    let bytes = encode_mouse_scroll(col, row, -3, sgr);
                    if !bytes.is_empty() {
                        self.mgr.send_input(bytes);
                    }
                } else if let Some(grid) = self.mgr.buffer_mut(&pane_id) {
                    grid.scroll_down(3);
                }
            }
            _ => {}
        }
    }

    fn handle_resize(&mut self, rows: u16, cols: u16) {
        // Account for session bar (1 row) + status bar (1 row) + hint bar (1 row)
        let term_rows = rows.saturating_sub(3);
        let term_cols = cols;

        if let Some(pane_id) = self.mgr.active_pane_id().map(|s| s.to_string()) {
            self.mgr.send_resize(&pane_id, term_rows, term_cols);
        }
    }

    /// Query the current terminal size, accounting for UI chrome (3 rows).
    fn current_term_size() -> TermSize {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        TermSize {
            rows: rows.saturating_sub(3),
            cols,
        }
    }

    /// Write a per-connection metadata log on first successful authentication.
    fn write_connection_log(&self) {
        let connected_at = {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Format as a basic ISO 8601 UTC timestamp (no chrono dependency)
            let (y, mo, d, h, mi, s) = epoch_secs_to_ymd_hms(secs);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        };
        let content = format!(
            "instance_id: {}\nclient_version: {}\nserver_version: {}\nprotocol_version: {}\ndestination: {}:{}\ntransport: QUIC\nconnected_at: {}\n",
            self.instance_id,
            env!("CARGO_PKG_VERSION"),
            self.mgr.server_version.as_deref().unwrap_or("unknown"),
            PROTOCOL_VERSION,
            self.mgr.host(),
            self.mgr.port(),
            connected_at,
        );
        match kmux_protocol::dirs::connection_log_path(&self.instance_id) {
            Ok(path) => {
                if let Err(e) = std::fs::write(&path, &content) {
                    tracing::warn!("Failed to write connection log {}: {e}", path.display());
                }
            }
            Err(e) => tracing::warn!("Failed to get connection log path: {e}"),
        }
    }
}

/// Returns the reconnect delay for the given attempt number.
/// Sequence: 1s, 2s, 4s, 8s, 30s (capped).
fn backoff_delay(attempt: u32) -> Duration {
    let secs = match attempt {
        0 => 1,
        1 => 2,
        2 => 4,
        3 => 8,
        _ => 30,
    };
    Duration::from_secs(secs)
}

/// Convert Unix timestamp (seconds) to (year, month, day, hour, minute, second) UTC.
fn epoch_secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    // Days since epoch
    let days = secs / 86400;
    let time = secs % 86400;
    let h = (time / 3600) as u32;
    let mi = ((time % 3600) / 60) as u32;
    let s = (time % 60) as u32;

    // Gregorian calendar calculation
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y } as u32;
    (y, mo, d, h, mi, s)
}
