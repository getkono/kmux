//! Pointer input on the grid: scroll-wheel handling and drag text selection.
//! Mirrors the TUI's `app/mouse_handler.rs` (scroll); selection is net-new
//! (the TUI has no mouse selection).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    DrawingArea, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    GestureClick, gdk, glib,
};

use kmux_client::grid::{GridPos, Selection, SelectionMode};

use crate::Frontend;

/// Lines scrolled per wheel notch (matches the TUI).
const SCROLL_LINES: i32 = 3;

/// Attach the pointer controllers (scroll wheel + drag selection) to the grid.
pub fn attach(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    attach_scroll(drawing, fe);
    attach_selection(drawing, fe);
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

/// Map a pointer pixel to an absolute [`GridPos`] on the *live* screen. Returns
/// `None` while scrolled back (selecting history is a follow-up) or with no
/// active grid. Row 0 of the absolute coordinate space is the oldest scrollback
/// line, so a visible row maps to `scrollback_len() + visible_row`.
fn pos_at(f: &Frontend, x: f64, y: f64) -> Option<GridPos> {
    let grid = f.core.mgr.active_grid()?;
    if grid.scroll_offset() != 0 {
        return None;
    }
    let col = ((x / f.metrics.cell_w).floor().max(0.0) as usize).min(grid.cols.saturating_sub(1));
    let vr = ((y / f.metrics.cell_h).floor().max(0.0) as usize).min(grid.rows.saturating_sub(1));
    Some(GridPos {
        row: grid.scrollback_len() + vr,
        col,
    })
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
/// double-click selects the word, triple-click selects the line. Copy is the
/// existing Ctrl+Shift+C path (`CopySelection` → `selected_text`).
fn attach_selection(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
    // The drag anchor is `Some` only while a single-click drag is in progress.
    let anchor: Rc<Cell<Option<GridPos>>> = Rc::new(Cell::new(None));

    let click = GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    {
        let fe = fe.clone();
        let area = drawing.clone();
        let anchor = anchor.clone();
        click.connect_pressed(move |_g, n_press, x, y| {
            let sel = {
                let f = fe.borrow();
                let Some(pos) = pos_at(&f, x, y) else {
                    anchor.set(None);
                    return;
                };
                match n_press {
                    1 => {
                        anchor.set(Some(pos));
                        Selection {
                            anchor: pos,
                            end: pos,
                            mode: SelectionMode::Normal,
                        }
                    }
                    2 => {
                        anchor.set(None);
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
                        anchor.set(None);
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
        let anchor = anchor.clone();
        click.connect_released(move |_g, _n, _x, _y| anchor.set(None));
    }
    drawing.add_controller(click);

    // Extend the selection while a single-click drag is active.
    let motion = EventControllerMotion::new();
    {
        let fe = fe.clone();
        let area = drawing.clone();
        motion.connect_motion(move |_m, x, y| {
            let Some(a) = anchor.get() else {
                return;
            };
            let end = {
                let f = fe.borrow();
                pos_at(&f, x, y)
            };
            if let Some(end) = end {
                set_selection(
                    &fe,
                    &area,
                    Some(Selection {
                        anchor: a,
                        end,
                        mode: SelectionMode::Normal,
                    }),
                );
            }
        });
    }
    drawing.add_controller(motion);
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
