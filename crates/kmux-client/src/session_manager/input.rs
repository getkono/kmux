use kmux_protocol::messages::{ClientMessage, KeyEvent, TermSize};
use kmux_protocol::parse_pane_id;

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

    /// Whether a terminal-output frame for `pane_id` is currently being withheld
    /// from this pane because it is exempt from auto-pause (issue #68) — i.e.
    /// the pane is in the exempt-panes set or its session is exempt.
    pub fn is_pane_auto_pause_exempt(&self, pane_id: &str) -> bool {
        if self.auto_pause_exempt_panes.contains(pane_id) {
            return true;
        }
        parse_pane_id(pane_id)
            .map(|(word_id, _)| self.auto_pause_exempt_sessions.contains(word_id))
            .unwrap_or(false)
    }

    /// Whether the pane was explicitly marked exempt at the *pane* level (drives
    /// the pane menu's checkmark; session-level exemption is reported separately).
    pub fn pane_marked_auto_pause_exempt(&self, pane_id: &str) -> bool {
        self.auto_pause_exempt_panes.contains(pane_id)
    }

    /// Whether `word_id` was marked exempt at the *session* level (menu checkmark).
    pub fn session_marked_auto_pause_exempt(&self, word_id: &str) -> bool {
        self.auto_pause_exempt_sessions.contains(word_id)
    }

    /// Reconcile the connection pause state with the daemon (issue #68).
    ///
    /// `paused` is the effective pause, `auto` whether it is the background
    /// auto-pause (vs a manual pause). Sends `SetPaused` only when the connection
    /// state changes, then re-attaches every visible pane that transitions from
    /// *skipped* to *streaming* so the daemon sends one fresh snapshot of the
    /// final state — instant, bounded catch-up over the well-tested snapshot
    /// path. A pane exempt from auto-pause keeps streaming through a background
    /// pause, so it never needs catch-up there.
    pub fn reconcile_pause(&mut self, paused: bool, auto: bool) {
        let (was_paused, was_auto) = self.pause_applied;
        if (paused, auto) != self.pause_applied {
            self.send_ws(ClientMessage::SetPaused { paused, auto });
        }
        // A pane's output is withheld iff the connection is paused and the pause
        // is not an auto-pause the pane is exempt from. Re-attach panes that flip
        // from withheld to streaming (`attach_fresh` re-asserts the exemption).
        let withheld = |paused: bool, auto: bool, exempt: bool| paused && !(auto && exempt);
        for pane_id in self.visible_panes.clone() {
            let exempt = self.is_pane_auto_pause_exempt(&pane_id);
            if withheld(was_paused, was_auto, exempt) && !withheld(paused, auto, exempt) {
                self.attach_fresh(pane_id);
            }
        }
        self.pause_applied = (paused, auto);
    }

    /// Toggle a pane's exemption from auto-pause and return the new state. Sends
    /// `SetPaneNoAutoPause` immediately if the pane is currently attached; the
    /// caller follows with [`Self::reconcile_pause`] so a pane that starts/stops
    /// streaming under the live pause state is re-synced (issue #68).
    pub fn toggle_pane_auto_pause_exempt(&mut self, pane_id: &str) -> bool {
        let exempt = !self.auto_pause_exempt_panes.contains(pane_id);
        if exempt {
            self.auto_pause_exempt_panes.insert(pane_id.to_string());
        } else {
            self.auto_pause_exempt_panes.remove(pane_id);
        }
        if self.visible_panes.iter().any(|p| p == pane_id) {
            self.send_ws(ClientMessage::SetPaneNoAutoPause {
                pane_id: pane_id.to_string(),
                exempt,
            });
        }
        exempt
    }

    /// Toggle a whole session's exemption from auto-pause and return the new
    /// state. Asserts the change for every currently-visible pane of the session.
    pub fn toggle_session_auto_pause_exempt(&mut self, word_id: &str) -> bool {
        let exempt = !self.auto_pause_exempt_sessions.contains(word_id);
        if exempt {
            self.auto_pause_exempt_sessions.insert(word_id.to_string());
        } else {
            self.auto_pause_exempt_sessions.remove(word_id);
        }
        for pane_id in self.visible_panes.clone() {
            let in_session = parse_pane_id(&pane_id)
                .map(|(w, _)| w == word_id)
                .unwrap_or(false);
            if in_session {
                self.send_ws(ClientMessage::SetPaneNoAutoPause { pane_id, exempt });
            }
        }
        exempt
    }
}
