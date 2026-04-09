use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{Event, EventStream, KeyEvent, MouseEvent, MouseEventKind};
use futures::StreamExt;
use kmux_client::input::{encode_mouse_scroll, key_to_bytes};
use kmux_client::session_manager::{SessionEvent, SessionManager};
use kmux_protocol::messages::{PROTOCOL_VERSION, ServerMessage, SessionEntry};
use ratatui::Terminal;
use ratatui::prelude::CrosstermBackend;
use tokio::sync::mpsc;
use tracing::info;

use crate::key_convert;
use crate::mode::{self, Action, ConnectField, Mode};
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

    /// Width (in columns) of the session badge in the top bar, used to detect
    /// mouse clicks that should open the session picker.
    pub session_badge_cols: u16,

    needs_render: bool,

    /// Unique ID for this client process, written to the connection log on auth success.
    instance_id: String,
}

impl App {
    pub fn new(
        host: String,
        port: u16,
        token: String,
        accept_invalid_certs: bool,
        is_local: bool,
        initial_cwd: String,
        instance_id: String,
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

        Self {
            mgr: SessionManager::new(host, port, token, accept_invalid_certs),
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
        }
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        let mut event_stream = EventStream::new();
        let (srv_tx, mut srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let mut reconnect_timer: Option<tokio::time::Instant> = None;

        // Auto-connect if token is available
        if !self.connect_token.is_empty() {
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
                                self.handle_session_events(events);
                            }
                            self.needs_render = true;
                        }
                        None => {
                            // Channel closed = disconnected
                            if self.mgr.connected {
                                self.mgr.mark_connection_lost();
                                self.disconnect_at = Some(Instant::now());
                                reconnect_timer = Some(
                                    tokio::time::Instant::now() + Duration::from_secs(3),
                                );
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
                    self.mgr.set_status_msg("Reconnecting...".to_string());
                    let (new_tx, new_rx) = mpsc::unbounded_channel();
                    srv_rx = new_rx;
                    self.mgr.connect(new_tx).await;
                    self.needs_render = true;
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
                        if self.is_local {
                            let cwd = self.initial_cwd.clone();
                            if let Some(word_id) = self.mgr.find_session_by_cwd(&cwd) {
                                self.mgr.select_session(word_id);
                            } else {
                                self.mgr.create_session_with_cwd(&cwd);
                            }
                        } else {
                            // Remote: show directory picker pre-filled with local CWD
                            self.dir_picker_buffer = self.initial_cwd.clone();
                            self.mode = Mode::DirectoryPicker;
                        }
                    }
                }
                _ => {}
            }
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
                self.mgr.create_session();
            }
            Action::CreatePane => {
                self.mgr.create_pane();
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
                            self.mgr.create_session_with_cwd(&cwd);
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
