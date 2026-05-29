use crossterm::event::{KeyEvent, KeyEventKind};

use crate::key_convert;
use crate::mode::{self, Action};

use super::{App, KeyResult};

impl App {
    /// Handle a key event. Returns the appropriate `KeyResult` for the event loop.
    ///
    /// Two things stay frontend-side: converting the crossterm event to the
    /// toolkit-agnostic key, and the `ForwardKey` / clipboard I/O. Everything
    /// else is delegated to `AppCore::dispatch_action`.
    pub(super) async fn handle_key(&mut self, key_event: KeyEvent) -> KeyResult {
        // Drop key-release events.  These appear when the host terminal has
        // kitty `report_events` enabled (we don't enable it ourselves but
        // some terminals are sticky).  Forwarding them would double-fire
        // every keystroke through the resolver.
        if matches!(key_event.kind, KeyEventKind::Release) {
            return KeyResult::Continue;
        }

        let (key, mods) = key_convert::convert(&key_event);
        let (new_mode, action) = mode::resolve(&self.mode, &key, mods);

        if let Some(m) = new_mode {
            self.mode = m;
        }

        // ForwardKey requires the original event so the daemon can encode it
        // under the live Ghostty mode state — handle it here rather than in the
        // toolkit-agnostic core dispatch (which treats it as a no-op).
        if matches!(action, Action::ForwardKey) {
            // Snap to bottom on keypress.
            if let Some(grid) = self.mgr.active_grid_mut() {
                grid.scroll_to_bottom();
            }
            if let Some(proto) = key_convert::convert_to_protocol_key(&key_event) {
                self.mgr.send_key_batch(vec![proto]);
            }
            return KeyResult::Continue;
        }

        let result = self.dispatch_action(action).await;
        self.apply_clipboard_effect(result)
    }

    /// Perform the toolkit-specific clipboard I/O for the clipboard effects the
    /// core emits. These are handled here (using `arboard`) and collapsed to
    /// `Continue` so they never reach the event loop. A GUI frontend would
    /// implement the same two effects with its own clipboard API.
    fn apply_clipboard_effect(&mut self, result: KeyResult) -> KeyResult {
        match result {
            KeyResult::CopyToClipboard(text) => {
                tokio::task::spawn_blocking(move || {
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(text);
                    }
                });
                KeyResult::Continue
            }
            KeyResult::RequestPaste => {
                if let Some(tx) = self.paste_tx.clone() {
                    tokio::task::spawn_blocking(move || {
                        if let Ok(mut cb) = arboard::Clipboard::new()
                            && let Ok(text) = cb.get_text()
                        {
                            let _ = tx.send(text);
                        }
                    });
                }
                KeyResult::Continue
            }
            other => other,
        }
    }
}
