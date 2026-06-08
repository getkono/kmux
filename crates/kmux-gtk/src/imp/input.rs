//! Pointer input on the grid: scroll-wheel handling and drag text selection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{
    DrawingArea, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    GestureClick, gdk, glib,
};

use kmux_client::grid::{GridPos, Selection, SelectionMode};

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
    attach_selection(drawing, fe);
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
            let col = (px / f.metrics.cell_w).max(0.0) as u16;
            let row = (py / f.metrics.cell_h).max(0.0) as u16;
            scroll_pane(&mut f, col, row, lines);
        }
        area.queue_draw();
        glib::Propagation::Stop
    });
    drawing.add_controller(scroll);
}

/// Map a pointer pixel to an absolute [`GridPos`], accounting for the current
/// scroll position so selection works while scrolled into history. Returns
/// `None` only when there is no active grid. Pixels outside the grid are clamped
/// to the nearest edge cell by `CellGrid::visible_to_abs`.
fn pos_at(f: &Frontend, x: f64, y: f64) -> Option<GridPos> {
    let grid = f.core.mgr.active_grid()?;
    let col = (x / f.metrics.cell_w).floor().max(0.0) as usize;
    let vr = (y / f.metrics.cell_h).floor().max(0.0) as usize;
    Some(grid.visible_to_abs(vr, col))
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

    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let drag = drag.clone();
        click.connect_pressed(move |_g, n_press, x, y| {
            // Clicking the terminal takes keyboard focus back from the sidebar.
            area.grab_focus();
            let sel = {
                let f = fe.borrow();
                let Some(pos) = pos_at(&f, x, y) else {
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
                            .map(|g| g.find_word_boundaries(pos))
                            .unwrap_or((pos, pos));
                        Selection {
                            anchor: s,
                            end: e,
                            mode: SelectionMode::Word,
                        }
                    }
                    _ => {
                        drag.set(None);
                        let cols = f.core.mgr.active_grid().map(|g| g.cols).unwrap_or(1);
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
        let drag = drag.clone();
        click.connect_released(move |_g, _n, _x, _y| drag.set(None));
    }
    drawing.add_controller(click);

    // Extend the selection — and record the pointer position for auto-scroll —
    // while a single-click drag is active.
    let motion = EventControllerMotion::new();
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let drag = drag.clone();
        motion.connect_motion(move |_m, x, y| {
            let Some(mut d) = drag.get() else {
                return;
            };
            d.last_x = x;
            d.last_y = y;
            drag.set(Some(d));
            let end = {
                let f = fe.borrow();
                pos_at(&f, x, y)
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
    let height = area.height() as f64;
    let mut f = fe.borrow_mut();
    let cell_w = f.metrics.cell_w;
    let Some(grid) = f.core.mgr.active_grid_mut() else {
        return;
    };
    let cols = grid.cols;
    let rows = grid.rows;
    let col = ((d.last_x / cell_w).floor().max(0.0) as usize).min(cols.saturating_sub(1));
    let edge_vr = if d.last_y < AUTO_SCROLL_MARGIN {
        grid.scroll_up(AUTO_SCROLL_LINES);
        0
    } else if d.last_y > height - AUTO_SCROLL_MARGIN {
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

/// Apply a wheel scroll to the active pane (port of the TUI `scroll_pane`).
fn scroll_pane(f: &mut Frontend, col: u16, row: u16, lines: i32) {
    let Some(pane_id) = f.core.mgr.active_pane_id().map(|s| s.to_string()) else {
        return;
    };
    let use_pty = f
        .core
        .mgr
        .buffer(&pane_id)
        .map(|g| g.modes().mouse_report())
        .unwrap_or(false);
    if use_pty {
        let sgr = f
            .core
            .mgr
            .buffer(&pane_id)
            .map(|g| g.modes().sgr_mouse())
            .unwrap_or(false);
        // 1-based terminal coordinates.
        let bytes = kmux_client::input::encode_mouse_scroll(col + 1, row + 1, lines, sgr);
        if !bytes.is_empty() {
            f.core.mgr.send_input(bytes);
        }
    } else if let Some(grid) = f.core.mgr.buffer_mut(&pane_id) {
        if lines > 0 {
            grid.scroll_up(lines as usize);
        } else {
            grid.scroll_down((-lines) as usize);
        }
    }
}
