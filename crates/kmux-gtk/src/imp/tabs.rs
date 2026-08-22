//! Tab strip: an `adw::TabView` + `adw::TabBar` reconciled against the active
//! session's **tabs** (Session → Tab → Pane). One GTK tab page per kmux tab.
//!
//! The active tab's panes are drawn tiled into the single shared `DrawingArea`
//! (see `render::render_tiled`), which is reparented into the selected page.
//! Selecting a tab calls `select_tab`; closing one closes the tab server-side
//! and vetoes the immediate GTK removal (the reconcile drops the page once the
//! tab is gone), so there is never a page without a live tab behind it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Application, Box as GtkBox, Orientation, glib};
use kmux_protocol::format_pane_id;

use super::Frontend;
use super::shell::Shell;

/// `tab_index` ↔ `TabPage` mapping (the index stringified) plus a re-entrancy
/// guard so the page changes we make programmatically don't echo back as a
/// select/close, and a signature so terminal output doesn't churn the strip
/// every frame.
pub struct TabState {
    map: RefCell<Vec<(String, adw::TabPage)>>,
    syncing: Cell<bool>,
    sig: RefCell<Option<String>>,
}

impl TabState {
    pub fn new() -> Self {
        Self {
            map: RefCell::new(Vec::new()),
            syncing: Cell::new(false),
            sig: RefCell::new(None),
        }
    }
}

/// Look up the `tab_index` mapped to `page`.
fn tab_index_for(s: &Shell, page: &adw::TabPage) -> Option<u32> {
    s.tabs
        .map
        .borrow()
        .iter()
        .find(|(_, p)| p.eq(page))
        .and_then(|(id, _)| id.parse::<u32>().ok())
}

/// Connect the `TabView` signals to the shared interaction policy. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, _app: &Application) {
    // Selecting a tab → view that tab (attach its pane set, focus its pane).
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.tab_view.connect_selected_page_notify(move |tv| {
            if s.tabs.syncing.get() {
                return;
            }
            let Some(page) = tv.selected_page() else {
                return;
            };
            let Some(tab_index) = tab_index_for(&s, &page) else {
                return;
            };
            {
                let mut f = fe.borrow_mut();
                f.core.mgr.select_tab(tab_index);
                f.core.request_render();
            }
            show_in_page(&s, &page);
            s.drawing.grab_focus();
            s.drawing.queue_draw();
        });
    }

    // Closing a tab → close it server-side and veto the immediate GTK removal;
    // the reconcile removes the page once the tab is gone.
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.tab_view.connect_close_page(move |tv, page| {
            if s.tabs.syncing.get() {
                // We initiated this close in the reconcile; allow it.
                tv.close_page_finish(page, true);
                return glib::Propagation::Stop;
            }
            if let Some(tab_index) = tab_index_for(&s, page) {
                let mut f = fe.borrow_mut();
                f.core.mgr.close_tab_index(tab_index);
                f.core.request_render();
            }
            // Server-authoritative: keep the page until the tab actually closes.
            tv.close_page_finish(page, false);
            glib::Propagation::Stop
        });
    }
}

/// Reconcile the tab strip against `mgr.active_session_tabs()`. Cheap no-op when
/// the tab set + active tab are unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, want, active_pane) = {
        let f = fe.borrow();
        let mgr = &f.core.mgr;
        let active = mgr.active_tab().map(|t| t.to_string()).unwrap_or_default();
        let word = mgr
            .active_session()
            .map(ToString::to_string)
            .unwrap_or_default();
        let want: Vec<(String, String)> = mgr
            .active_session_tabs()
            .iter()
            .map(|t| {
                // A tab is paused if any of its panes is paused (issue #68).
                let paused = t
                    .layout
                    .leaves()
                    .iter()
                    .any(|idx| f.core.is_pane_paused(&format_pane_id(&word, *idx)));
                (
                    t.tab_index.to_string(),
                    tab_label(t.tab_index, &t.name, paused),
                )
            })
            .collect();
        let sig = format!(
            "{active}|{}",
            want.iter()
                .map(|(id, t)| format!("{id}:{t};"))
                .collect::<String>()
        );
        (sig, want, active)
    };
    if shell.tabs.sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *shell.tabs.sig.borrow_mut() = Some(sig);

    shell.tabs.syncing.set(true);

    if want.is_empty() {
        let pages: Vec<adw::TabPage> = shell
            .tabs
            .map
            .borrow()
            .iter()
            .map(|(_, p)| p.clone())
            .collect();
        for p in pages {
            shell.tab_view.close_page(&p);
        }
        shell.tabs.map.borrow_mut().clear();
        detach_drawing(shell);
        shell.content_stack.set_visible_child_name("empty");
        shell.tab_bar.set_visible(false);
        shell.tabs.syncing.set(false);
        return;
    }

    // First pane appearing (empty → panes) takes keyboard focus so typing works
    // without a click.
    let was_empty = shell.content_stack.visible_child_name().as_deref() != Some("panes");
    shell.content_stack.set_visible_child_name("panes");
    shell.tab_bar.set_visible(true);

    // Drop pages whose pane is gone.
    let stale: Vec<adw::TabPage> = shell
        .tabs
        .map
        .borrow()
        .iter()
        .filter(|(id, _)| !want.iter().any(|(wid, _)| wid == id))
        .map(|(_, p)| p.clone())
        .collect();
    for p in stale {
        shell.tab_view.close_page(&p);
    }
    shell
        .tabs
        .map
        .borrow_mut()
        .retain(|(id, _)| want.iter().any(|(wid, _)| wid == id));

    // Add new pages and refresh titles, in pane order.
    for (id, label) in &want {
        let existing = shell
            .tabs
            .map
            .borrow()
            .iter()
            .find(|(eid, _)| eid == id)
            .map(|(_, p)| p.clone());
        match existing {
            Some(page) => page.set_title(label),
            None => {
                let boxw = GtkBox::new(Orientation::Vertical, 0);
                let page = shell.tab_view.append(&boxw);
                page.set_title(label);
                shell.tabs.map.borrow_mut().push((id.clone(), page));
            }
        }
    }

    // Select and show the active pane.
    let active_page = shell
        .tabs
        .map
        .borrow()
        .iter()
        .find(|(id, _)| *id == active_pane)
        .map(|(_, p)| p.clone());
    if let Some(page) = active_page {
        shell.tab_view.set_selected_page(&page);
        show_in_page(shell, &page);
        if was_empty {
            shell.drawing.grab_focus();
        }
    }

    shell.tabs.syncing.set(false);
}

/// Tab title: the tab's name, falling back to its 1-based index. A paused tab is
/// prefixed with a pause glyph (issue #68).
fn tab_label(index: u32, name: &str, paused: bool) -> String {
    let base = if name.trim().is_empty() {
        format!("{}", index + 1)
    } else {
        name.to_string()
    };
    if paused { format!("⏸ {base}") } else { base }
}

/// Move the single shared `DrawingArea` into `page`'s content box.
fn show_in_page(shell: &Shell, page: &adw::TabPage) {
    let Ok(boxw) = page.child().downcast::<GtkBox>() else {
        return;
    };
    if let Some(parent) = shell.drawing.parent()
        && let Ok(cur) = parent.downcast::<GtkBox>()
    {
        if cur == boxw {
            return;
        }
        cur.remove(&shell.drawing);
    }
    boxw.append(&shell.drawing);
}

/// Detach the shared `DrawingArea` from any tab page (empty-state).
fn detach_drawing(shell: &Shell) {
    if let Some(parent) = shell.drawing.parent()
        && let Ok(boxw) = parent.downcast::<GtkBox>()
    {
        boxw.remove(&shell.drawing);
    }
}
