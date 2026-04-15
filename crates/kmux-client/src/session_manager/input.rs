use kmux_protocol::messages::{ClientMessage, TermSize};

use super::SessionManager;

impl SessionManager {
    /// Send raw PTY input bytes for the active pane.
    pub fn send_input(&mut self, data: Vec<u8>) -> bool {
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this pane".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyInput { pane_id, data });
        }
        true
    }

    /// Send a paste string for the active pane.
    pub fn send_paste(&mut self, text: String) -> bool {
        if text.is_empty() {
            return true;
        }
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.status_msg = "Input locked on this pane".to_string();
                return false;
            }
            self.send_ws(ClientMessage::PtyPaste {
                pane_id,
                data: text,
            });
        }
        true
    }

    /// Send a resize event for the given pane and resize the local buffer.
    pub fn send_resize(&mut self, pane_id: &str, rows: u16, cols: u16) {
        if let Some(buf) = self.buffers.get_mut(pane_id) {
            buf.resize(rows, cols);
        }
        self.send_ws(ClientMessage::Resize {
            pane_id: pane_id.to_string(),
            size: TermSize { rows, cols },
        });
    }

    /// Send a Unix signal to the PTY child of the active pane.
    pub fn send_signal(&mut self, pane_id: &str, signal: i32) {
        self.send_ws(ClientMessage::Signal {
            pane_id: pane_id.to_string(),
            signal,
        });
    }

    /// Toggle the input lock on the active pane.
    pub fn toggle_input_lock(&mut self) {
        if let Some(pane_id) = self.active_pane.clone() {
            let locked = self.input_locked.get(&pane_id).copied().unwrap_or(false);
            if locked {
                self.send_ws(ClientMessage::ReleaseInputLock { pane_id });
            } else {
                self.send_ws(ClientMessage::RequestInputLock { pane_id });
            }
        }
    }

    /// Enable or disable full-snapshot mode on the server.
    pub fn set_snapshot_mode(&mut self, enabled: bool) {
        self.send_ws(ClientMessage::SetSnapshotMode { enabled });
    }
}
