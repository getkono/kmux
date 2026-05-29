//! Native overlay widgets shown over the grid, driven by `core.mode`.
//!
//! Like the chrome bars, overlays are *views* of `AppCore` state. A single
//! centered modal box is rebuilt by [`sync`] when a state signature changes;
//! input still flows through the key controller → `mode::resolve` →
//! `dispatch_action`, so the overlays render state and the keyboard drives it.
//! Buttons (mouse affordances) call the same effects. Mirrors the TUI's
//! `ui/overlays/`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Align, Application, Box as GtkBox, Button, DrawingArea, EventControllerMotion, Label,
    Orientation, Overlay,
};

use kmux_app::core::{AppCore, KeyResult};
use kmux_app::mode::{Action, Mode};

use crate::{Frontend, handle_effect};

/// Persistent overlay widgets plus the last-rendered signature.
pub struct Overlays {
    /// Centered modal card for the mode-driven overlays.
    modal: GtkBox,
    last_sig: RefCell<Option<String>>,
}

/// Build the overlay widgets (hidden) and add them to `overlay`.
pub fn build(overlay: &Overlay) -> Overlays {
    let modal = GtkBox::new(Orientation::Vertical, 6);
    modal.add_css_class("kmux-overlay");
    modal.set_halign(Align::Center);
    modal.set_valign(Align::Center);
    modal.set_visible(false);
    overlay.add_overlay(&modal);
    Overlays {
        modal,
        last_sig: RefCell::new(None),
    }
}

/// Fingerprint of the overlay-relevant state. Includes picker/command buffers so
/// the modal rebuilds as the user types or moves the selection.
fn signature(core: &AppCore) -> String {
    format!(
        "{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        core.mode,
        core.hud_visible,
        core.metrics_overlay_visible,
        core.session_picker_search,
        core.session_picker_selected,
        core.server_picker_search,
        core.server_picker_selected,
        core.dir_picker_buffer,
        core.dir_picker_selected,
        core.mgr.session_list().len(),
    )
}

fn clear(b: &GtkBox) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

/// Rebuild the modal from the current mode if the signature changed.
pub fn sync(ov: &Overlays, fe: &Rc<RefCell<Frontend>>, app: &Application, drawing: &DrawingArea) {
    let sig = signature(&fe.borrow().core);
    if ov.last_sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *ov.last_sig.borrow_mut() = Some(sig);

    clear(&ov.modal);
    let mode = fe.borrow().core.mode.clone();
    match mode {
        Mode::Connecting { target_display } => {
            connecting(&ov.modal, &target_display, fe, drawing);
            ov.modal.set_visible(true);
        }
        Mode::Disconnected { reason } => {
            disconnected(&ov.modal, &reason, fe, app, drawing);
            ov.modal.set_visible(true);
        }
        Mode::SessionPicker => {
            ov.modal.set_size_request(460, -1);
            session_picker(&ov.modal, fe, app, drawing);
            ov.modal.set_visible(true);
        }
        Mode::ServerPicker => {
            ov.modal.set_size_request(460, -1);
            server_picker(&ov.modal, fe, app, drawing);
            ov.modal.set_visible(true);
        }
        Mode::DirectoryPicker => {
            ov.modal.set_size_request(460, -1);
            dir_picker(&ov.modal, fe, app, drawing);
            ov.modal.set_visible(true);
        }
        // Command palette, help, confirm, rename, HUD, and metrics follow.
        _ => {
            ov.modal.set_size_request(-1, -1);
            ov.modal.set_visible(false);
        }
    }
}

/// A search/path input line: dim prefix, the buffer, and an accent caret.
fn input_line(prefix: &str, buffer: &str) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 0);
    row.set_halign(Align::Start);
    row.append(&label(prefix, "kmux-overlay-dim"));
    let buf = Label::new(Some(buffer));
    buf.set_halign(Align::Start);
    row.append(&buf);
    row.append(&label("\u{258f}", "kmux-overlay-caret"));
    row
}

/// One selectable picker row: click activates it, hover highlights it. `idx` is
/// the value fed to `set_picker_selected` (row index for the session picker
/// where 0 is "new session", else the match index).
fn picker_row(
    text: &str,
    selected: bool,
    idx: usize,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) -> Button {
    let b = Button::with_label(text);
    b.set_can_focus(false);
    b.set_halign(Align::Fill);
    b.add_css_class("kmux-overlay-row");
    if selected {
        b.add_css_class("selected");
    }
    if let Some(lbl) = b.child().and_downcast::<Label>() {
        lbl.set_halign(Align::Start);
        lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    }
    {
        let fe = fe.clone();
        let app = app.clone();
        let drawing = drawing.clone();
        b.connect_clicked(move |_| {
            let result = {
                let mut f = fe.borrow_mut();
                f.core.set_picker_selected(idx);
                let r = f.core.activate_picker_selection();
                f.core.needs_render = true;
                r
            };
            if let Some(result) = result {
                handle_effect(&fe, result, &app, &drawing);
            }
            drawing.queue_draw();
        });
    }
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        let motion = EventControllerMotion::new();
        motion.connect_enter(move |_, _, _| {
            {
                let mut f = fe.borrow_mut();
                f.core.set_picker_selected(idx);
                f.core.needs_render = true;
            }
            drawing.queue_draw();
        });
        b.add_controller(motion);
    }
    b
}

fn session_picker(
    modal: &GtkBox,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) {
    let f = fe.borrow();
    let core = &f.core;
    modal.append(&label(" Sessions ", "kmux-overlay-title"));
    modal.append(&input_line("Search: ", &core.session_picker_search));

    let selected = core.session_picker_selected;
    // Row 0 is the synthetic "new session" affordance; matches occupy 1..N+1.
    let mut rows: Vec<(usize, String)> = vec![(0, "[+] New session\u{2026}".to_string())];
    for (i, e) in core.session_picker_matches().iter().enumerate().take(12) {
        let name = core.mgr.display_name_for(&e.meta.word_id);
        rows.push((
            i + 1,
            format!("{name:<20} {}p  {}", e.panes.len(), e.meta.cwd),
        ));
    }
    for (idx, text) in rows {
        modal.append(&picker_row(&text, idx == selected, idx, fe, app, drawing));
    }
}

fn server_picker(
    modal: &GtkBox,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) {
    let f = fe.borrow();
    let core = &f.core;
    modal.append(&label(" Servers ", "kmux-overlay-title"));
    modal.append(&input_line("Search: ", &core.server_picker_search));

    let selected = core.server_picker_selected;
    let servers = core.filtered_servers();
    if servers.is_empty() {
        modal.append(&label("(no recent servers)", "kmux-overlay-dim"));
    }
    for (i, s) in servers.iter().enumerate().take(12) {
        let text = format!("{:<28} {}s  {}", s.display, s.sessions.len(), s.time_ago());
        modal.append(&picker_row(&text, i == selected, i, fe, app, drawing));
    }
}

fn dir_picker(
    modal: &GtkBox,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) {
    let f = fe.borrow();
    let core = &f.core;
    modal.append(&label(" Open Session ", "kmux-overlay-title"));
    modal.append(&input_line("Directory: ", &core.dir_picker_buffer));

    let matches = core.dir_picker_matches();
    if matches.is_empty() {
        modal.append(&label(
            "(no existing sessions — Enter to create)",
            "kmux-overlay-dim",
        ));
    }
    let selected = core.dir_picker_selected;
    for (i, e) in matches.iter().enumerate().take(12) {
        let name = core.mgr.display_name_for(&e.meta.word_id);
        let text = format!("{name:<16} {}", e.meta.cwd);
        modal.append(&picker_row(&text, i == selected, i, fe, app, drawing));
    }
}

fn label(text: &str, css: &str) -> Label {
    let l = Label::new(Some(text));
    l.add_css_class(css);
    l.set_halign(Align::Start);
    l
}

/// A non-focusable button so keyboard input keeps flowing to the grid.
fn action_button(text: &str) -> Button {
    let b = Button::with_label(text);
    b.set_can_focus(false);
    b
}

fn connecting(modal: &GtkBox, target: &str, fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    modal.append(&label("Connecting…", "kmux-overlay-title"));
    modal.append(&label(target, "kmux-overlay-dim"));
    modal.append(&label("Esc to cancel", "kmux-overlay-dim"));

    let cancel = action_button("Cancel");
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        cancel.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                // CancelBootstrap drops cancel_tx; the pump then transitions to
                // Disconnected. dispatch_action never awaits, so block_on is fine.
                let _ =
                    futures::executor::block_on(f.core.dispatch_action(Action::CancelBootstrap));
                f.core.needs_render = true;
            }
            drawing.queue_draw();
        });
    }
    modal.append(&cancel);
}

fn disconnected(
    modal: &GtkBox,
    reason: &str,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) {
    modal.append(&label("Disconnected", "kmux-overlay-title"));
    modal.append(&label(reason, "kmux-overlay-error"));
    modal.append(&label("Enter to reconnect · q to quit", "kmux-overlay-dim"));

    let row = GtkBox::new(Orientation::Horizontal, 8);
    let reconnect = action_button("Reconnect");
    {
        let fe = fe.clone();
        let app = app.clone();
        let drawing = drawing.clone();
        reconnect.connect_clicked(move |_| {
            handle_effect(&fe, KeyResult::Reconnect, &app, &drawing);
        });
    }
    let quit = action_button("Quit");
    {
        let app = app.clone();
        quit.connect_clicked(move |_| app.quit());
    }
    row.append(&reconnect);
    row.append(&quit);
    modal.append(&row);
}
