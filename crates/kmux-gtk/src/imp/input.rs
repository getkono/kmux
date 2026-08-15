//! Pointer input on the grid: scroll-wheel handling and drag text selection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    Align, DrawingArea, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    EventSequenceState, GestureClick, GestureDrag, PopoverMenu, gdk, gio, glib,
};

use kmux_app::layout::{Divider, ratios_for_drag};
use kmux_client::grid::{GridPos, Selection, SelectionMode};
use kmux_client::input::{MouseButton, MouseEvent, MouseEventKind, MouseMods};
use kmux_protocol::messages::SplitDir;

use super::Frontend;

/// Lines scrolled per wheel notch (matches the TUI).
const SCROLL_LINES: i32 = 3;

/// Distance (logical px) from the top/bottom grid edge within which a held drag
/// starts auto-scrolling so the selection can run past the viewport.
const AUTO_SCROLL_MARGIN: f64 = 8.0;

/// Display rows scrolled per auto-scroll tick (~60 Hz → a brisk but controllable
/// drag-scroll).
const AUTO_SCROLL_LINES: usize = 2;

/// Auto-scroll cadence, matching the render pump's 16 ms.
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(16);

/// Attach the pointer controllers (scroll wheel + drag selection + click-to-
/// focus) to the grid.
pub fn attach(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    attach_focus_click(drawing, fe);
    attach_scroll(drawing, fe);
    attach_resize(drawing, fe);
    attach_context_menu(drawing, fe);
    attach_selection(drawing, fe);
}

/// Right-click context menu on a pane: focus the pane under the pointer (so the
/// items target it) and pop a menu of the existing `win.*` pane actions. The
/// popover is parented to the grid once and re-pointed on each press.
fn attach_context_menu(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    let menu = gio::Menu::new();
    menu.append(Some("Split Right"), Some("win.split-right"));
    menu.append(Some("Split Down"), Some("win.split-down"));
    menu.append(Some("Zoom Pane"), Some("win.zoom-pane"));
    menu.append(Some("Close Pane"), Some("win.close-pane"));
    // Keep this pane streaming through a background auto-pause (issue #68).
    menu.append(
        Some("Keep Streaming in Background"),
        Some("win.toggle-pane-keep-streaming"),
    );
    let popover = PopoverMenu::from_model(Some(&menu));
    popover.set_parent(drawing);
    popover.set_has_arrow(false);
    popover.set_halign(Align::Start);

    let gesture = GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    let fe = fe.clone();
    let area = drawing.clone();
    gesture.connect_pressed(move |_g, _n, x, y| {
        let mut changed = false;
        if let Some(pane_id) = super::tiles::pane_at(&fe, x, y, area.width(), area.height()) {
            let mut f = fe.borrow_mut();
            if f.core.mgr.active_pane_id() != Some(pane_id.as_str()) {
                f.core.mgr.focus_pane(pane_id);
                f.core.needs_render = true;
                changed = true;
            }
        }
        if changed {
            area.queue_draw();
        }
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    });
    drawing.add_controller(gesture);
}

/// Divider interaction: a hover over a divider shows a resize cursor, and a
/// primary-button drag on a divider adjusts the owning split's ratios live
/// (reusing the keyboard-resize wire path, [`kmux_client::session_manager::SessionManager::set_layout_ratios`]
/// via `ratios_for_drag`). Double-click-to-reset and text-selection suppression
/// on dividers live in [`attach_selection`]'s press handler.
fn attach_resize(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    // Hover: show a col-/row-resize cursor over a divider, default elsewhere.
    let motion = EventControllerMotion::new();
    {
        let fe = fe.clone();
        let area = drawing.clone();
        motion.connect_motion(move |_m, x, y| {
            let name = {
                let f = fe.borrow();
                super::tiles::divider_at(&f, x, y, area.width(), area.height()).map(|d| {
                    match d.dir {
                        SplitDir::Horizontal => "col-resize",
                        SplitDir::Vertical => "row-resize",
                    }
                })
            };
            match name {
                Some(n) => area.set_cursor(gdk::Cursor::from_name(n, None).as_ref()),
                None => area.set_cursor(None),
            }
        });
    }
    {
        let area = drawing.clone();
        motion.connect_leave(move |_m| area.set_cursor(None));
    }
    drawing.add_controller(motion);

    // Drag: a press that begins on a divider claims the sequence (so selection
    // doesn't also start) and resizes the split as the pointer moves.
    let active: Rc<RefCell<Option<Divider>>> = Rc::new(RefCell::new(None));
    let drag = GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let active = active.clone();
        drag.connect_drag_begin(move |g, x, y| {
            let d = {
                let f = fe.borrow();
                super::tiles::divider_at(&f, x, y, area.width(), area.height())
            };
            if d.is_some() {
                g.set_state(EventSequenceState::Claimed);
            }
            *active.borrow_mut() = d;
        });
    }
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let active = active.clone();
        drag.connect_drag_update(move |g, ox, oy| {
            let Some(div) = active.borrow().clone() else {
                return;
            };
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            let (px, py) = (sx + ox, sy + oy);
            let mut f = fe.borrow_mut();
            let pointer_cell = match div.dir {
                SplitDir::Horizontal => (px / f.metrics.cell_w).max(0.0) as u16,
                SplitDir::Vertical => (py / f.metrics.cell_h).max(0.0) as u16,
            };
            // Recompute against the *current* tree each update so a concurrent
            // LayoutUpdate can't desync the drag (ratios_for_drag no-ops if the
            // split was reshaped).
            let Some(layout) = f.core.mgr.render_layout() else {
                return;
            };
            if let Some(ratios) = ratios_for_drag(&layout, &div, pointer_cell) {
                f.core.mgr.set_layout_ratios(div.path.clone(), ratios);
                f.core.needs_render = true;
            }
            drop(f);
            area.queue_draw();
        });
    }
    {
        let active = active.clone();
        drag.connect_drag_end(move |_g, _ox, _oy| {
            *active.borrow_mut() = None;
        });
    }
    drawing.add_controller(drag);
}

/// Click-to-focus: a primary-button press focuses the tiled pane under the
/// pointer. Does not claim the event, so drag-selection still starts. No-op for
/// a single-pane tab (the click already lands on the only pane).
fn attach_focus_click(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    let click = GestureClick::new();
    let fe = fe.clone();
    let area = drawing.clone();
    click.connect_pressed(move |_g, _n, x, y| {
        let (w, h) = (area.width(), area.height());
        if let Some(pane_id) = super::tiles::pane_at(&fe, x, y, w, h) {
            let mut f = fe.borrow_mut();
            if f.core.mgr.active_pane_id() != Some(pane_id.as_str()) {
                f.core.mgr.focus_pane(pane_id);
                f.core.needs_render = true;
            }
        }
        area.grab_focus();
    });
    drawing.add_controller(click);
}

/// Scroll wheel. Like the TUI: when the inner program enabled mouse reporting,
/// the wheel is encoded as an SGR/X10 mouse event and sent to the PTY;
/// otherwise it scrolls the local scrollback.
fn attach_scroll(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    let scroll = EventControllerScroll::new(
        EventControllerScrollFlags::VERTICAL | EventControllerScrollFlags::DISCRETE,
    );
    let fe = fe.clone();
    let area = drawing.clone();
    scroll.connect_scroll(move |ctl, _dx, dy| {
        if dy == 0.0 {
            return glib::Propagation::Proceed;
        }
        // GTK: dy < 0 is wheel-up. The TUI convention is lines > 0 = scroll up
        // (toward history).
        let lines = if dy < 0.0 {
            SCROLL_LINES
        } else {
            -SCROLL_LINES
        };
        let (px, py) = ctl
            .current_event()
            .and_then(|e| e.position())
            .unwrap_or((0.0, 0.0));
        {
            let mut f = fe.borrow_mut();
            // Scroll the pane under the pointer (in its local cells), not the
            // focused one.
            if let Some((pane_id, rect)) =
                super::tiles::pane_hit(&f, px, py, area.width(), area.height())
            {
                let col = ((px / f.metrics.cell_w) - rect.col as f64).max(0.0) as u16;
                let row = ((py / f.metrics.cell_h) - rect.row as f64).max(0.0) as u16;
                scroll_pane(&mut f, &pane_id, col, row, lines);
            }
        }
        area.queue_draw();
        glib::Propagation::Stop
    });
    drawing.add_controller(scroll);
}

/// Map a pointer pixel to an absolute [`GridPos`] in the focused pane, accounting
/// for the pane's offset within a tiled tab and the current scroll position so
/// selection works inside any tile and while scrolled into history. Returns
/// `None` only when there is no active grid. Pixels outside the grid are clamped
/// to the nearest edge cell by `CellGrid::visible_to_abs`.
fn pos_at(f: &Frontend, x: f64, y: f64, width_px: i32, height_px: i32) -> Option<GridPos> {
    let grid = f.core.mgr.active_grid()?;
    let (off_c, off_r) = super::tiles::focused_rect(f, width_px, height_px)
        .map_or((0.0, 0.0), |r| (r.col as f64, r.row as f64));
    let col = ((x / f.metrics.cell_w) - off_c).floor().max(0.0) as usize;
    let vr = ((y / f.metrics.cell_h) - off_r).floor().max(0.0) as usize;
    Some(grid.visible_to_abs(vr, col))
}

/// Map a pointer pixel to a 0-based *visible viewport* cell in the focused pane,
/// clamped to the grid, for forwarding mouse reports to the inner program. The
/// program only knows its on-screen grid, never the scrollback, so — unlike
/// [`pos_at`] — this does not go through `visible_to_abs`. Returns `None` when
/// there is no active grid.
fn viewport_cell(
    f: &Frontend,
    x: f64,
    y: f64,
    width_px: i32,
    height_px: i32,
) -> Option<(u16, u16)> {
    let grid = f.core.mgr.active_grid()?;
    let (off_c, off_r) = super::tiles::focused_rect(f, width_px, height_px)
        .map_or((0.0, 0.0), |r| (r.col as f64, r.row as f64));
    let col = (((x / f.metrics.cell_w) - off_c).floor().max(0.0) as usize)
        .min(grid.cols.saturating_sub(1));
    let row = (((y / f.metrics.cell_h) - off_r).floor().max(0.0) as usize)
        .min(grid.rows.saturating_sub(1));
    Some((col as u16, row as u16))
}

/// Build [`MouseMods`] from a GTK modifier state. `keep_shift` is `false` for
/// motion/release on an in-progress PTY drag (the forward-vs-select decision was
/// already made at press, so a Shift pressed mid-drag must not strand it).
fn mods_from_state(st: gdk::ModifierType, keep_shift: bool) -> MouseMods {
    MouseMods {
        ctrl: st.contains(gdk::ModifierType::CONTROL_MASK),
        alt: st.contains(gdk::ModifierType::ALT_MASK),
        shift: keep_shift && st.contains(gdk::ModifierType::SHIFT_MASK),
    }
}

/// In-progress single-click drag: the fixed anchor plus the last pointer
/// position. The position lets a held drag past the viewport edge keep
/// auto-scrolling from the last known location even after motion events stop
/// firing (the pointer left the widget).
#[derive(Clone, Copy)]
struct Drag {
    anchor: GridPos,
    last_x: f64,
    last_y: f64,
}

/// Set the active grid's selection and repaint.
fn set_selection(fe: &Rc<RefCell<Frontend>>, area: &DrawingArea, sel: Option<Selection>) {
    {
        let mut f = fe.borrow_mut();
        if let Some(g) = f.core.mgr.active_grid_mut() {
            g.set_selection(sel);
        }
    }
    area.queue_draw();
}

/// Left-button drag selection: single-click anchors and drags to extend,
/// double-click selects the word, triple-click selects the line. A drag held at
/// the top/bottom edge auto-scrolls so a selection can span more than one
/// screen. Copy is the existing Ctrl+Shift+C path (`CopySelection` →
/// `selected_text`).
fn attach_selection(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    // `Some` only while a single-click drag is in progress.
    let drag: Rc<Cell<Option<Drag>>> = Rc::new(Cell::new(None));
    // `true` while a primary-button drag is being forwarded to a mouse-tracking
    // inner program (so motion/release forward too and local selection is
    // suppressed). Mutually exclusive with `drag`.
    let pty_drag: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let drag = drag.clone();
        let pty_drag = pty_drag.clone();
        click.connect_pressed(move |g, n_press, x, y| {
            // Clicking the terminal takes keyboard focus back from the sidebar.
            area.grab_focus();
            // A press on a divider is a resize, not a text selection: suppress
            // selection here (the GestureDrag handles the drag), and reset the
            // split to even on a double-click.
            if let Some(div) = {
                let f = fe.borrow();
                super::tiles::divider_at(&f, x, y, area.width(), area.height())
            } {
                if n_press == 2 {
                    let mut f = fe.borrow_mut();
                    if let Some(layout) = f.core.mgr.render_layout()
                        && let Some(ratios) = kmux_app::layout::even_ratios_at(&layout, &div.path)
                    {
                        f.core.mgr.set_layout_ratios(div.path.clone(), ratios);
                        f.core.needs_render = true;
                    }
                }
                drag.set(None);
                return;
            }
            // If the focused pane's program enabled mouse tracking (and Shift
            // isn't held to force local selection), forward the press to the PTY
            // instead of starting a selection. Forwarding any press regardless of
            // `n_press` lets the program interpret its own multi-clicks.
            if let Some((col, row)) = {
                let f = fe.borrow();
                viewport_cell(&f, x, y, area.width(), area.height())
            } {
                let mods = mods_from_state(g.current_event_state(), true);
                let forwarded = {
                    let mut f = fe.borrow_mut();
                    f.core.mgr.report_mouse(
                        false,
                        MouseEvent {
                            button: MouseButton::Left,
                            kind: MouseEventKind::Press,
                            col: col + 1,
                            row: row + 1,
                            mods,
                        },
                    )
                };
                if forwarded {
                    pty_drag.set(true);
                    drag.set(None);
                    // Drop any lingering highlight so the program owns the screen.
                    set_selection(&fe, &area, None);
                    return;
                }
            }
            let sel = {
                let f = fe.borrow();
                let Some(pos) = pos_at(&f, x, y, area.width(), area.height()) else {
                    drag.set(None);
                    return;
                };
                match n_press {
                    1 => {
                        drag.set(Some(Drag {
                            anchor: pos,
                            last_x: x,
                            last_y: y,
                        }));
                        Selection {
                            anchor: pos,
                            end: pos,
                            mode: SelectionMode::Normal,
                        }
                    }
                    2 => {
                        drag.set(None);
                        let (s, e) = f
                            .core
                            .mgr
                            .active_grid()
                            .map_or((pos, pos), |g| g.find_word_boundaries(pos));
                        Selection {
                            anchor: s,
                            end: e,
                            mode: SelectionMode::Word,
                        }
                    }
                    _ => {
                        drag.set(None);
                        let cols = f.core.mgr.active_grid().map_or(1, |g| g.cols);
                        Selection {
                            anchor: GridPos {
                                row: pos.row,
                                col: 0,
                            },
                            end: GridPos {
                                row: pos.row,
                                col: cols.saturating_sub(1),
                            },
                            mode: SelectionMode::Line,
                        }
                    }
                }
            };
            set_selection(&fe, &area, Some(sel));
        });
    }
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let drag = drag.clone();
        let pty_drag = pty_drag.clone();
        click.connect_released(move |g, _n, x, y| {
            if pty_drag.replace(false) {
                if let Some((col, row)) = {
                    let f = fe.borrow();
                    viewport_cell(&f, x, y, area.width(), area.height())
                } {
                    let mods = mods_from_state(g.current_event_state(), false);
                    let mut f = fe.borrow_mut();
                    f.core.mgr.report_mouse(
                        false,
                        MouseEvent {
                            button: MouseButton::Left,
                            kind: MouseEventKind::Release,
                            col: col + 1,
                            row: row + 1,
                            mods,
                        },
                    );
                }
                return;
            }
            drag.set(None);
        });
    }
    drawing.add_controller(click);

    // Extend the selection — and record the pointer position for auto-scroll —
    // while a single-click drag is active.
    let motion = EventControllerMotion::new();
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let drag = drag.clone();
        let pty_drag = pty_drag.clone();
        motion.connect_motion(move |m, x, y| {
            // While forwarding a drag to a mouse-tracking program, report motion
            // to the PTY (gated server-side: 1002 needs a button held, which we
            // are; 1000 reports none). No local selection in this mode.
            if pty_drag.get() {
                if let Some((col, row)) = {
                    let f = fe.borrow();
                    viewport_cell(&f, x, y, area.width(), area.height())
                } {
                    let mods = mods_from_state(m.current_event_state(), false);
                    let mut f = fe.borrow_mut();
                    f.core.mgr.report_mouse(
                        true,
                        MouseEvent {
                            button: MouseButton::Left,
                            kind: MouseEventKind::Motion,
                            col: col + 1,
                            row: row + 1,
                            mods,
                        },
                    );
                }
                return;
            }
            let Some(mut d) = drag.get() else {
                return;
            };
            d.last_x = x;
            d.last_y = y;
            drag.set(Some(d));
            let end = {
                let f = fe.borrow();
                pos_at(&f, x, y, area.width(), area.height())
            };
            if let Some(end) = end {
                set_selection(
                    &fe,
                    &area,
                    Some(Selection {
                        anchor: d.anchor,
                        end,
                        mode: SelectionMode::Normal,
                    }),
                );
            }
        });
    }
    drawing.add_controller(motion);

    // Auto-scroll pump: always-on but cheap (a no-op unless a drag sits at an
    // edge), mirroring the render pump. A single persistent timer avoids the
    // double-fire hazard of per-drag timers.
    {
        let fe = fe.clone();
        let area = drawing.clone();
        glib::timeout_add_local(AUTO_SCROLL_INTERVAL, move || {
            autoscroll_tick(&fe, &area, &drag);
            glib::ControlFlow::Continue
        });
    }
}

/// One auto-scroll step: if a drag sits within [`AUTO_SCROLL_MARGIN`] of the
/// top/bottom grid edge, scroll the active grid and extend the selection to the
/// edge cell under the pointer's last column.
fn autoscroll_tick(fe: &Rc<RefCell<Frontend>>, area: &DrawingArea, drag: &Rc<Cell<Option<Drag>>>) {
    let Some(d) = drag.get() else {
        return;
    };
    let (w, h) = (area.width(), area.height());
    let mut f = fe.borrow_mut();
    let cell_w = f.metrics.cell_w;
    let cell_h = f.metrics.cell_h;
    // Auto-scroll relative to the focused pane's tile, not the whole window, so
    // a drag near a tile edge (mid-window) scrolls correctly.
    let rect = super::tiles::focused_rect(&f, w, h);
    let off_c = rect.map_or(0.0, |r| r.col as f64);
    let pane_top = rect.map_or(0.0, |r| r.row as f64 * cell_h);
    let pane_bottom = rect.map_or(h as f64, |r| (r.row + r.rows) as f64 * cell_h);
    let Some(grid) = f.core.mgr.active_grid_mut() else {
        return;
    };
    let cols = grid.cols;
    let rows = grid.rows;
    let col = (((d.last_x / cell_w) - off_c).floor().max(0.0) as usize).min(cols.saturating_sub(1));
    let edge_vr = if d.last_y < pane_top + AUTO_SCROLL_MARGIN {
        grid.scroll_up(AUTO_SCROLL_LINES);
        0
    } else if d.last_y > pane_bottom - AUTO_SCROLL_MARGIN {
        grid.scroll_down(AUTO_SCROLL_LINES);
        rows.saturating_sub(1)
    } else {
        return;
    };
    let end = grid.visible_to_abs(edge_vr, col);
    grid.set_selection(Some(Selection {
        anchor: d.anchor,
        end,
        mode: SelectionMode::Normal,
    }));
    drop(f);
    area.queue_draw();
}

/// Apply a wheel scroll to `pane_id` (the pane under the pointer), in that
/// pane's local cell coordinates (port of the TUI `scroll_pane`). Mouse-report
/// is forwarded to the PTY only when the pointer is over the focused pane (input
/// is routed to the focused pane); over any other pane the wheel scrolls that
/// pane's local scrollback.
fn scroll_pane(f: &mut Frontend, pane_id: &str, col: u16, row: u16, lines: i32) {
    let focused = f.core.mgr.active_pane_id() == Some(pane_id);
    let use_pty = focused
        && f.core
            .mgr
            .buffer(pane_id)
            .is_some_and(|g| g.modes().mouse_report());
    if use_pty {
        let sgr = f
            .core
            .mgr
            .buffer(pane_id)
            .is_some_and(|g| g.modes().sgr_mouse());
        // 1-based terminal coordinates.
        let bytes = kmux_client::input::encode_mouse_scroll(col + 1, row + 1, lines, sgr);
        if !bytes.is_empty() {
            f.core.mgr.send_input(bytes);
        }
    } else if let Some(grid) = f.core.mgr.buffer_mut(pane_id) {
        if lines > 0 {
            grid.scroll_up(lines as usize);
        } else {
            grid.scroll_down((-lines) as usize);
        }
    }
}
