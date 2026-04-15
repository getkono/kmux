use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kmux_client::session_manager::SessionEvent;
use kmux_protocol::messages::{PROTOCOL_VERSION, SessionEntry, TermSize};
use tracing::info;

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
pub(super) fn backoff_delay(attempt: u32) -> Duration {
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
