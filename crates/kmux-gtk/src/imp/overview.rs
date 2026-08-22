//! Process overview (issue #122): a main-area view, swapped in over the terminal
//! when [`Mode::ProcessOverview`] is active, listing every session's
//! Tab → Pane → Process tree with CPU/memory. The rows come from the
//! toolkit-agnostic [`kmux_app::core::AppCore::overview_rows`] projection; this module only
//! renders them into a `GtkListBox` and is reconciled by the pump while the view
//! is open (the driver re-requests the snapshot at ~1 Hz, issue #122).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Align, Box as GtkBox, EventControllerKey, Label, ListBox, ListBoxRow, Orientation,
    ScrolledWindow, SelectionMode, gdk, glib, pango,
};

use kmux_app::core::{OverviewRow, OverviewRowKind};
use kmux_app::mode::{Action, Mode};

use super::Frontend;
use super::shell::Shell;

const COL_CPU_CHARS: i32 = 6;
const COL_MEM_CHARS: i32 = 9;
const COL_PID_CHARS: i32 = 7;

/// Build the overview stack child (a header row + a scrolled, reconcilable list)
/// and return it alongside the list to repopulate.
pub fn build() -> (GtkBox, ListBox) {
    let container = GtkBox::new(Orientation::Vertical, 0);
    container.append(&header_row());

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::None);
    list.add_css_class("rich-list");
    let placeholder = Label::new(Some("No active sessions"));
    placeholder.add_css_class("dim-label");
    placeholder.set_margin_top(12);
    list.set_placeholder(Some(&placeholder));

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    container.append(&scroll);

    (container, list)
}

/// Wire Esc / `q` on the overview list to close the view (the action toggles,
/// returning to the terminal). Called once after the `Frontend` exists, mirroring
/// the terminal key controller setup.
pub fn attach_keys(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let keys = EventControllerKey::new();
    let fe = fe.clone();
    let drawing = shell.drawing.clone();
    keys.connect_key_pressed(move |_c, keyval, _code, _mods| {
        if matches!(keyval, gdk::Key::Escape | gdk::Key::q) {
            {
                let mut f = fe.borrow_mut();
                f.core.dispatch_action(Action::ToggleProcessOverview);
                f.core.request_render();
            }
            drawing.grab_focus();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    shell.overview_list.add_controller(keys);
}

/// Reconcile the overview list while [`Mode::ProcessOverview`] is active, and
/// make it the visible stack child. A no-op in every other mode (so the normal
/// panes/empty switching in `tabs::sync` stands).
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let rows = {
        let f = fe.borrow();
        if !matches!(f.core.mode, Mode::ProcessOverview) {
            return;
        }
        f.core.overview_rows()
    };

    while let Some(child) = shell.overview_list.first_child() {
        shell.overview_list.remove(&child);
    }
    for r in &rows {
        shell.overview_list.append(&make_row(r));
    }

    shell.content_stack.set_visible_child_name("overview");
    shell.overview_list.grab_focus();
}

/// The column header strip above the list.
fn header_row() -> GtkBox {
    let hb = data_row_box();
    hb.add_css_class("dim-label");
    let name = Label::new(Some("NAME"));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    hb.append(&name);
    hb.append(&header_cell("CPU%", COL_CPU_CHARS));
    hb.append(&header_cell("MEM", COL_MEM_CHARS));
    hb.append(&header_cell("PID", COL_PID_CHARS));
    hb
}

fn header_cell(text: &str, chars: i32) -> Label {
    let l = Label::new(Some(text));
    l.set_xalign(1.0);
    l.set_width_chars(chars);
    l
}

/// One list row for an [`OverviewRow`], indented by depth with right-aligned
/// CPU / memory / PID columns.
fn make_row(r: &OverviewRow) -> ListBoxRow {
    let hb = data_row_box();

    let name = Label::new(Some(&r.label));
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.set_margin_start(r.depth as i32 * 16);
    match r.kind {
        OverviewRowKind::Session => name.add_css_class("heading"),
        OverviewRowKind::Tab => name.add_css_class("caption-heading"),
        OverviewRowKind::Process => name.add_css_class("dim-label"),
        OverviewRowKind::Pane => {}
    }
    hb.append(&name);
    hb.append(&num_cell(&format!("{:.1}", r.cpu_percent), COL_CPU_CHARS));
    hb.append(&num_cell(
        &kmux_app::humanize::bytes_compact(r.mem_bytes),
        COL_MEM_CHARS,
    ));
    hb.append(&num_cell(
        &r.pid.map(|p| p.to_string()).unwrap_or_default(),
        COL_PID_CHARS,
    ));

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

fn num_cell(text: &str, chars: i32) -> Label {
    let l = Label::new(Some(text));
    l.set_xalign(1.0);
    l.set_width_chars(chars);
    l.set_halign(Align::End);
    l.add_css_class("numeric");
    l
}
