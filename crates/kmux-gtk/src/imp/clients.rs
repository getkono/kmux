//! Connected-clients view (issue #146): a main-area view, swapped in over the
//! terminal when [`Mode::ConnectedClients`] is active, listing the client
//! connections attached to the active session — label, machine id, hostname,
//! transport, panes — with a per-row **Kick** button. Rows come from the
//! toolkit-agnostic [`kmux_app::core::AppCore::client_rows`] projection; this module renders them
//! into a `GtkListBox` and is reconciled by the pump while the view is open (the
//! driver re-requests the list at ~1 Hz, issue #146).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, EventControllerKey, Label, ListBox, ListBoxRow, Orientation,
    ScrolledWindow, SelectionMode, gdk, glib, pango,
};

use kmux_app::mode::{Action, Mode};
use kmux_protocol::messages::ClientInfo;

use super::Frontend;
use super::shell::Shell;

const COL_MACHINE_CHARS: i32 = 14;
const COL_HOST_CHARS: i32 = 16;
const COL_TRANSPORT_CHARS: i32 = 9;
const COL_PANES_CHARS: i32 = 7;

/// Build the connected-clients stack child (a header row + a scrolled,
/// reconcilable list) and return it alongside the list to repopulate.
pub fn build() -> (GtkBox, ListBox) {
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.append(&header_row());

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("rich-list");
    let placeholder = Label::new(Some("No connected clients"));
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(12);
    list.set_placeholder(Some(&placeholder));

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    container.append(&scroll);

    (container, list)
}

/// Wire Esc / `q` on the clients list to close the view (toggling back to the
/// terminal). Called once after the `Frontend` exists, mirroring `overview`.
pub fn attach_keys(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let keys = EventControllerKey::new();
    let fe = fe.clone();
    let drawing = shell.drawing.clone();
    keys.connect_key_pressed(move |_c, keyval, _code, _mods| {
        if matches!(keyval, gdk::Key::Escape | gdk::Key::q) {
            {
                let mut f = fe.borrow_mut();
                f.core.dispatch_action(Action::ToggleConnectedClients);
                f.core.request_render();
            }
            drawing.grab_focus();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    shell.clients_list.add_controller(keys);
}

/// Reconcile the clients list while [`Mode::ConnectedClients`] is active and make
/// it the visible stack child. A no-op in every other mode.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let rows = {
        let f = fe.borrow();
        if !matches!(f.core.mode, Mode::ConnectedClients) {
            return;
        }
        f.core.client_rows()
    };

    while let Some(child) = shell.clients_list.first_child() {
        shell.clients_list.remove(&child);
    }
    for r in &rows {
        shell.clients_list.append(&make_row(r, fe, shell));
    }

    shell.content_stack.set_visible_child_name("clients");
    shell.clients_list.grab_focus();
}

/// The column header strip above the list.
fn header_row() -> GtkBox {
    let hb = data_row_box();
    hb.add_css_class("dim-label");
    let name = Label::new(Some("CLIENT"));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    hb.append(&name);
    hb.append(&header_cell("MACHINE", COL_MACHINE_CHARS));
    hb.append(&header_cell("HOST", COL_HOST_CHARS));
    hb.append(&header_cell("TRANSPORT", COL_TRANSPORT_CHARS));
    hb.append(&header_cell("PANES", COL_PANES_CHARS));
    hb
}

fn header_cell(text: &str, chars: i32) -> Label {
    let l = Label::new(Some(text));
    l.set_xalign(1.0);
    l.set_width_chars(chars);
    l
}

/// One list row for a [`ClientInfo`], with a trailing Kick button (disabled for
/// the requester's own connection).
fn make_row(r: &ClientInfo, fe: &Rc<RefCell<Frontend>>, shell: &Rc<Shell>) -> ListBoxRow {
    let hb = data_row_box();

    let label = if r.is_self {
        format!("{} (you)", r.label)
    } else {
        r.label.clone()
    };
    let name = Label::new(Some(&label));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(pango::EllipsizeMode::End);
    if r.is_self {
        name.add_css_class("heading");
    }
    hb.append(&name);
    hb.append(&text_cell(&short_id(&r.machine_id), COL_MACHINE_CHARS));
    hb.append(&text_cell(&r.hostname, COL_HOST_CHARS));
    hb.append(&text_cell(&r.transport, COL_TRANSPORT_CHARS));
    hb.append(&text_cell(&panes_text(&r.attached_panes), COL_PANES_CHARS));

    let kick = Button::with_label("Kick");
    kick.add_css_class("destructive-action");
    kick.set_halign(Align::End);
    if r.is_self {
        kick.set_sensitive(false);
    } else {
        let fe = fe.clone();
        let drawing = shell.drawing.clone();
        let client_id = r.client_id;
        kick.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                f.core.mutate(|c| c.kick_listed_client(client_id));
                f.core.request_render();
            }
            drawing.grab_focus();
        });
    }
    hb.append(&kick);

    let row = ListBoxRow::new();
    row.set_child(Some(&hb));
    row.set_activatable(false);
    row.set_selectable(false);
    row
}

fn data_row_box() -> GtkBox {
    let hb = GtkBox::new(Orientation::Horizontal, 8);
    hb.set_margin_top(2);
    hb.set_margin_bottom(2);
    hb.set_margin_start(8);
    hb.set_margin_end(8);
    hb
}

fn text_cell(text: &str, chars: i32) -> Label {
    let l = Label::new(Some(text));
    l.set_xalign(1.0);
    l.set_width_chars(chars);
    l.set_halign(Align::End);
    l.set_ellipsize(pango::EllipsizeMode::End);
    l
}

/// Abbreviated machine-id fingerprint for display (first 12 hex chars).
fn short_id(machine_id: &str) -> String {
    machine_id.chars().take(12).collect()
}

fn panes_text(panes: &[u32]) -> String {
    panes
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}
