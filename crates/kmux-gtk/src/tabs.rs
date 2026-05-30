//! Pane tabs: an `adw::TabView` + `adw::TabBar` reconciled against the active
//! session's panes. One tab per pane.
//!
//! The protocol streams only the *active* pane's grid (`mgr.active_grid()`), so a
//! single shared `DrawingArea` is reparented into the selected page rather than
//! one grid per page. Selecting a tab applies [`TopBarAction::SelectPane`];
//! closing one dispatches [`Action::ClosePane`] and lets the server confirm the
//! removal (the reconcile drops the page when the pane leaves the session), so
//! there is never a tab without a live pane behind it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Application, Box as GtkBox, Orientation, glib};

use kmux_app::core::TopBarAction;
use kmux_app::mode::Action;

use crate::Frontend;
use crate::shell::Shell;

/// `pane_id` ↔ `TabPage` mapping plus a re-entrancy guard so the page changes we
/// make programmatically don't echo back as `SelectPane`/`ClosePane`, and a
/// signature so terminal output doesn't churn the tab strip every frame.
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

/// Connect the `TabView` signals to the shared interaction policy. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, _app: &Application) {
    // Selecting a tab → make that pane active (the server then streams its grid).
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
            let pane_id = s
                .tabs
                .map
                .borrow()
                .iter()
                .find(|(_, p)| p.eq(&page))
                .map(|(id, _)| id.clone());
            let Some(pane_id) = pane_id else {
                return;
            };
            {
                let mut f = fe.borrow_mut();
                f.core
                    .apply_top_bar_action(TopBarAction::SelectPane(pane_id));
                f.core.needs_render = true;
            }
            show_in_page(&s, &page);
            s.drawing.grab_focus();
            s.drawing.queue_draw();
        });
    }

    // Closing a tab → close the pane server-side and veto the immediate GTK
    // removal; the reconcile removes the page once the pane is gone.
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.tab_view.connect_close_page(move |tv, page| {
            if s.tabs.syncing.get() {
                // We initiated this close in the reconcile; allow it.
                tv.close_page_finish(page, true);
                return glib::Propagation::Stop;
            }
            let pane_id = s
                .tabs
                .map
                .borrow()
                .iter()
                .find(|(_, p)| p.eq(page))
                .map(|(id, _)| id.clone());
            if let Some(pane_id) = pane_id {
                let mut f = fe.borrow_mut();
                f.core
                    .apply_top_bar_action(TopBarAction::SelectPane(pane_id));
                let _ = futures::executor::block_on(f.core.dispatch_action(Action::ClosePane));
                f.core.needs_render = true;
            }
            // Server-authoritative: keep the tab until the pane actually closes.
            tv.close_page_finish(page, false);
            glib::Propagation::Stop
        });
    }
}

/// Reconcile the tab strip against `mgr.active_session_panes()`. Cheap no-op when
/// the pane set + active pane are unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, want, active_pane) = {
        let f = fe.borrow();
        let mgr = &f.core.mgr;
        let active = mgr.active_pane_id().unwrap_or("").to_string();
        let want: Vec<(String, String)> = mgr
            .active_session_panes()
            .iter()
            .map(|p| (p.pane_id.clone(), pane_label(p.pane_index, &p.title)))
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

/// Tab title: the pane title, falling back to its 1-based index.
fn pane_label(index: u32, title: &str) -> String {
    if title.trim().is_empty() {
        format!("pane {}", index + 1)
    } else {
        title.to_string()
    }
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
