//! Pointer input on the grid: scroll-wheel handling (this commit) and drag text
//! selection (next). Mirrors the TUI's `app/mouse_handler.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{DrawingArea, EventControllerScroll, EventControllerScrollFlags, glib};

use crate::Frontend;

/// Lines scrolled per wheel notch (matches the TUI).
const SCROLL_LINES: i32 = 3;

/// Attach the scroll-wheel controller to the grid. Like the TUI: when the inner
/// program enabled mouse reporting, the wheel is encoded as an SGR/X10 mouse
/// event and sent to the PTY; otherwise it scrolls the local scrollback.
pub fn attach(drawing: &DrawingArea, fe: &Rc<RefCell<Frontend>>) {
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
