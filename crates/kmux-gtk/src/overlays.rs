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
use gtk4::{Align, Application, Box as GtkBox, Button, DrawingArea, Label, Orientation, Overlay};

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
        // Pickers, command palette, help, confirm, rename, HUD, and metrics
        // are added in following commits.
        _ => ov.modal.set_visible(false),
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
