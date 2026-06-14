use kmux_protocol::messages::{ClientMessage, KeyEvent, TermSize};

use super::SessionManager;
use crate::input::{MouseEvent, MouseEventKind, encode_mouse_button};

impl SessionManager {
    /// Returns `Ok(pane_id)` if input is allowed on the active pane, or sets
    /// `status_msg` and returns `Err(false)` if the pane is locked.
    fn active_pane_unlocked(&mut self) -> Result<String, bool> {
        let pane_id = match self.active_pane.clone() {
            Some(p) => p,
            None => return Err(true),
        };
        if self.input_locked.get(&pane_id).copied().unwrap_or(false) {
            self.status_msg = "Input locked on this pane".to_string();
            return Err(false);
        }
        Ok(pane_id)
    }

    /// Send raw PTY input bytes for the active pane.
    ///
    /// Used for paths where bytes are already produced (mouse-report wheels,
    /// out-of-band signals).  Use [`Self::send_key_batch`] for actual
    /// keystrokes so the daemon can encode them with live mode state.
    pub fn send_input(&mut self, data: Vec<u8>) -> bool {
        match self.active_pane_unlocked() {
            Ok(pane_id) => {
                self.send_ws(ClientMessage::PtyInput { pane_id, data });
                true
            }
            Err(ok) => ok,
        }
    }

    /// Forward a pointer event to the active pane's inner program when it has
    /// enabled mouse tracking, returning `true` iff bytes were sent — in which
    /// case the caller skips its own client-side text selection.
    ///
    /// The decision policy (shared by every frontend so they behave
    /// identically):
    /// - **Shift is the bypass key**: a shift-held event is never forwarded, so
    ///   the user can always select locally even inside a mouse-mode program.
    /// - No active pane, or the program enabled no mouse mode → not forwarded.
    /// - Motion is gated by the mode: any-event tracking (1003) reports every
    ///   move; button-event tracking (1002) reports motion only while a button
    ///   is held (`button_held`); plain click tracking (1000) reports none.
    /// - Otherwise the event is encoded (SGR per the program's 1006 state) and
    ///   sent to the PTY.
    pub fn report_mouse(&mut self, button_held: bool, mut ev: MouseEvent) -> bool {
        if ev.mods.shift {
            return false;
        }
        let Some(pane_id) = self.active_pane.clone() else {
            return false;
        };
        // Snapshot the pane's modes and dimensions (all `Copy`) so the borrow is
        // released before the `send_input` mutable borrow below.
        let Some((modes, cols, rows)) = self
            .buffer(&pane_id)
            .map(|g| (g.modes(), g.cols as u16, g.rows as u16))
        else {
            return false;
        };
        if !modes.mouse_report() {
            return false;
        }
        if ev.kind == MouseEventKind::Motion {
            let wants = modes.mouse_motion() || (modes.mouse_drag() && button_held);
            if !wants {
                return false;
            }
        }
        // Clamp to the on-screen grid (1-based) so an edge/gutter pixel can't
        // report a cell the program doesn't have.
        ev.col = ev.col.clamp(1, cols.max(1));
        ev.row = ev.row.clamp(1, rows.max(1));
        let bytes = encode_mouse_button(&ev, modes.sgr_mouse());
        self.send_input(bytes)
    }

    /// Send a batch of structured key events for the active pane.  The
    /// daemon encodes each event using its live Ghostty mode state so the
    /// bytes always match what the inner program negotiated (DECCKM, kitty
    /// kbd flags, modifyOtherKeys, …).
    pub fn send_key_batch(&mut self, events: Vec<KeyEvent>) -> bool {
        if events.is_empty() {
            return true;
        }
        match self.active_pane_unlocked() {
            Ok(pane_id) => {
                self.send_ws(ClientMessage::PtyKeyBatch { pane_id, events });
                true
            }
            Err(ok) => ok,
        }
    }

    /// Send a paste string for the active pane.
    pub fn send_paste(&mut self, text: String) -> bool {
        if text.is_empty() {
            return true;
        }
        match self.active_pane_unlocked() {
            Ok(pane_id) => {
                self.send_ws(ClientMessage::PtyPaste {
                    pane_id,
                    data: text,
                });
                true
            }
            Err(ok) => ok,
        }
    }

    /// Send a resize event for the given pane and resize the local buffer.
    pub fn send_resize(&mut self, pane_id: &str, rows: u16, cols: u16) {
        if let Some(buf) = self.buffers.get_mut(pane_id) {
            buf.resize(rows, cols);
        }
        self.send_ws(ClientMessage::Resize {
            pane_id: pane_id.to_string(),
            size: TermSize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
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

    /// Pause or resume terminal-output delivery for this connection (issue #68).
    ///
    /// While paused the daemon stops pushing terminal frames, saving bandwidth;
    /// the pane keeps running so nothing is lost. On resume we re-attach every
    /// visible pane so the daemon sends a fresh snapshot of the *final* state —
    /// instant, bounded catch-up (one snapshot, not a backlog replay), and it
    /// reuses the well-tested snapshot-sync path.
    pub fn set_paused(&mut self, paused: bool) {
        self.send_ws(ClientMessage::SetPaused { paused });
        if !paused {
            for pane_id in self.visible_panes.clone() {
                self.attach_fresh(pane_id);
            }
        }
    }
}
