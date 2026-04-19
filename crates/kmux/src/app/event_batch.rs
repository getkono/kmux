use crossterm::event::{Event, MouseEventKind};
use kmux_client::input::{encode_mouse_scroll, key_to_bytes};
use kmux_protocol::messages::TermSize;

use crate::{key_convert, mode::Action};

use super::{App, KeyResult, event_loop::RESIZE_DEBOUNCE};

impl App {
    /// Process a batch of crossterm events, coalescing compatible runs to reduce
    /// redundant `send_input` calls and grid mutations before a single redraw.
    ///
    /// Coalescing rules:
    /// - Consecutive `ForwardKey` events → bytes concatenated, single `send_input`.
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
        let mut fwd_bytes: Vec<u8> = Vec::new();
        let mut local_scroll: Option<(String, i32)> = None;

        for event in batch {
            match event {
                Event::Key(key_event) => {
                    let (key, mods) = key_convert::convert(&key_event);
                    let (_, action) = crate::mode::resolve(&self.mode, &key, mods);

                    if matches!(action, Action::ForwardKey) {
                        if let Some((pane_id, delta)) = local_scroll.take() {
                            self.apply_local_scroll_delta(&pane_id, delta);
                        }
                        let app_cursor = self
                            .mgr
                            .active_grid()
                            .map(|b| b.app_cursor())
                            .unwrap_or(false);
                        let text = key_convert::text_from_event(&key_event);
                        if let Some(bytes) = key_to_bytes(&key, mods, text.as_deref(), app_cursor) {
                            if fwd_bytes.is_empty()
                                && let Some(grid) = self.mgr.active_grid_mut()
                            {
                                grid.scroll_to_bottom();
                            }
                            fwd_bytes.extend_from_slice(&bytes);
                        }
                    } else {
                        if !fwd_bytes.is_empty() {
                            self.mgr.send_input(std::mem::take(&mut fwd_bytes));
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
                    if !fwd_bytes.is_empty() {
                        self.mgr.send_input(std::mem::take(&mut fwd_bytes));
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

        if !fwd_bytes.is_empty() {
            self.mgr.send_input(fwd_bytes);
        }
        if let Some((pane_id, delta)) = local_scroll {
            self.apply_local_scroll_delta(&pane_id, delta);
        }

        self.needs_render = true;
        KeyResult::Continue
    }
}
