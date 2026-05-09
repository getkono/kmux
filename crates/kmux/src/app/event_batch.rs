use crossterm::event::{Event, KeyEventKind, MouseEventKind};
use kmux_client::input::encode_mouse_scroll;
use kmux_protocol::messages::{KeyEvent as ProtoKeyEvent, TermSize};

use crate::{key_convert, mode::Action};

use super::{App, KeyResult, event_loop::RESIZE_DEBOUNCE};

impl App {
    /// Process a batch of crossterm events, coalescing compatible runs to reduce
    /// redundant network round-trips and grid mutations before a single redraw.
    ///
    /// Coalescing rules:
    /// - Consecutive `ForwardKey` events → batched into one `send_key_batch`.
    /// - Consecutive local-scroll (mouse wheel, mouse-report OFF) on the same pane →
    ///   summed into one `scroll_up`/`scroll_down`.
    /// - PTY-scroll events (mouse-report ON) → sent individually (position matters).
    /// - Resize events → update `pending_resize`/`resize_deadline` debounce state.
    ///
    /// A non-Continue `KeyResult` (Quit, Reconnect, SwitchServer) flushes all
    /// accumulators and returns immediately; the rest of the batch is dropped.
    pub(super) async fn process_input_batch(
        &mut self,
        batch: Vec<Event>,
        pending_resize: &mut Option<TermSize>,
        resize_deadline: &mut Option<tokio::time::Instant>,
    ) -> KeyResult {
        let mut fwd_keys: Vec<ProtoKeyEvent> = Vec::new();
        let mut local_scroll: Option<(String, i32)> = None;

        for event in batch {
            match event {
                Event::Key(key_event) => {
                    // Drop key-release events.  Forwarding them would
                    // double-fire every keystroke through mode::resolve.
                    if matches!(key_event.kind, KeyEventKind::Release) {
                        continue;
                    }

                    let (key, mods) = key_convert::convert(&key_event);
                    let (_, action) = crate::mode::resolve(&self.mode, &key, mods);

                    if matches!(action, Action::ForwardKey) {
                        if let Some((pane_id, delta)) = local_scroll.take() {
                            self.apply_local_scroll_delta(&pane_id, delta);
                        }
                        if let Some(proto) = key_convert::convert_to_protocol_key(&key_event) {
                            if fwd_keys.is_empty()
                                && let Some(grid) = self.mgr.active_grid_mut()
                            {
                                grid.scroll_to_bottom();
                            }
                            fwd_keys.push(proto);
                        }
                    } else {
                        if !fwd_keys.is_empty() {
                            self.mgr.send_key_batch(std::mem::take(&mut fwd_keys));
                        }
                        if let Some((pane_id, delta)) = local_scroll.take() {
                            self.apply_local_scroll_delta(&pane_id, delta);
                        }
                        let result = self.handle_key(key_event).await;
                        if !matches!(result, KeyResult::Continue) {
                            self.needs_render = true;
                            return result;
                        }
                    }
                }

                Event::Mouse(mouse_event) => {
                    if !fwd_keys.is_empty() {
                        self.mgr.send_key_batch(std::mem::take(&mut fwd_keys));
                    }
                    match mouse_event.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            let lines: i32 = if matches!(mouse_event.kind, MouseEventKind::ScrollUp)
                            {
                                3
                            } else {
                                -3
                            };
                            let pane_id = self.mgr.active_pane_id().map(|s| s.to_string());
                            if let Some(pid) = pane_id {
                                let use_pty = self
                                    .mgr
                                    .buffer(&pid)
                                    .map(|g| g.modes().mouse_report())
                                    .unwrap_or(false);
                                if use_pty {
                                    if let Some((sp, delta)) = local_scroll.take() {
                                        self.apply_local_scroll_delta(&sp, delta);
                                    }
                                    let sgr = self
                                        .mgr
                                        .buffer(&pid)
                                        .map(|g| g.modes().sgr_mouse())
                                        .unwrap_or(false);
                                    let bytes = encode_mouse_scroll(
                                        mouse_event.column + 1,
                                        mouse_event.row + 1,
                                        lines,
                                        sgr,
                                    );
                                    if !bytes.is_empty() {
                                        self.mgr.send_input(bytes);
                                    }
                                } else {
                                    match local_scroll.as_mut() {
                                        Some((sp, delta)) if *sp == pid => {
                                            *delta += lines;
                                        }
                                        Some(_) => {
                                            let (sp, delta) = local_scroll.take().unwrap();
                                            self.apply_local_scroll_delta(&sp, delta);
                                            local_scroll = Some((pid, lines));
                                        }
                                        None => {
                                            local_scroll = Some((pid, lines));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            if let Some((sp, delta)) = local_scroll.take() {
                                self.apply_local_scroll_delta(&sp, delta);
                            }
                            if let Some(result) = self.handle_mouse(mouse_event) {
                                self.needs_render = true;
                                return result;
                            }
                        }
                    }
                }

                Event::Resize(cols, rows) => {
                    *pending_resize = Some(App::compute_pane_size(rows, cols));
                    *resize_deadline = Some(tokio::time::Instant::now() + RESIZE_DEBOUNCE);
                }

                _ => {}
            }
        }

        if !fwd_keys.is_empty() {
            self.mgr.send_key_batch(fwd_keys);
        }
        if let Some((pane_id, delta)) = local_scroll {
            self.apply_local_scroll_delta(&pane_id, delta);
        }

        self.needs_render = true;
        KeyResult::Continue
    }
}
