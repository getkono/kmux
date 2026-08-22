//! Sessions sidebar: a `GtkListBox` of the live sessions, reconciled against
//! `mgr.session_list()`. Selecting a row jumps to that session; the header
//! "＋" button opens the unified launcher (issue #121). Federated sessions are
//! grouped under a per-remote header (local sessions under "Local") so the user
//! can tell at a glance which machine a session lives on. Optimized for rapid
//! swapping (always-visible one-click targets; `Ctrl+1..9` / `Ctrl+Tab`
//! accelerators live in `actions`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Align, Application, Label, ListBoxRow};

use kmux_app::core::TopBarAction;
use kmux_app::mode::Action;

use super::Frontend;
use super::shell::Shell;

/// Per-row → `session_list` index mapping (`None` for a section header), a
/// re-entrancy guard so programmatic selection doesn't echo back as a jump, and
/// a content signature to skip no-op rebuilds.
pub struct SidebarState {
    targets: RefCell<Vec<Option<usize>>>,
    syncing: Cell<bool>,
    sig: RefCell<Option<String>>,
}

impl SidebarState {
    pub fn new() -> Self {
        Self {
            targets: RefCell::new(Vec::new()),
            syncing: Cell::new(false),
            sig: RefCell::new(None),
        }
    }
}

/// Wire the sidebar interactions. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, _app: &Application) {
    // "＋ New session": open the unified launcher (issue #121) — pick a local
    // directory, an existing session, or a remote, rather than assuming a cwd.
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.new_session_btn.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                f.core.apply_top_bar_action(TopBarAction::OpenLaunchPicker);
                f.core.request_render();
            }
            s.drawing.queue_draw();
        });
    }

    // Row selected → jump to that session (mapping skips section headers).
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.sidebar_list.connect_row_selected(move |_lb, row| {
            if s.sidebar.syncing.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let target = s
                .sidebar
                .targets
                .borrow()
                .get(idx as usize)
                .copied()
                .flatten();
            let Some(si) = target else {
                return;
            };
            {
                let mut f = fe.borrow_mut();
                let _ = f.core.dispatch_action(Action::JumpToSession(si));
                f.core.request_render();
            }
            // Return focus to the terminal so typing goes to the session.
            s.drawing.grab_focus();
            s.drawing.queue_draw();
        });
    }
}

/// One reconciled display row: a non-selectable group header or a session.
enum Row {
    Header(String),
    Session {
        /// Index into `mgr.session_list()` (the `JumpToSession` argument).
        si: usize,
        name: String,
        cwd: String,
        panes: usize,
        active: bool,
    },
}

/// Reconcile the session rows, grouped by peer. Cheap no-op when the session set,
/// grouping, and active session are unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, rows, active_idx) = {
        let f = fe.borrow();
        let mgr = &f.core.mgr;
        let active = mgr.active_session();

        // Partition session_list indices into local + per-peer groups, preserving
        // session_list order within each group; BTreeMap gives a stable peer order.
        let mut local: Vec<usize> = Vec::new();
        let mut remotes: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (i, e) in mgr.session_list().iter().enumerate() {
            match e.peer.as_deref() {
                None => local.push(i),
                Some(p) => remotes.entry(p).or_default().push(i),
            }
        }

        // Build the flat display list. A purely local list needs no grouping
        // chrome; once a remote is federated, label every group (incl. "Local").
        let grouped = !remotes.is_empty();
        let mut rows: Vec<Row> = Vec::new();
        let push_session = |rows: &mut Vec<Row>, i: usize| {
            let e = &mgr.session_list()[i];
            rows.push(Row::Session {
                si: i,
                name: mgr.display_name_for(&e.meta.word_id),
                cwd: e.meta.cwd.clone(),
                panes: e.panes.len(),
                active: active == Some(e.meta.word_id.as_str()),
            });
        };
        if grouped && !local.is_empty() {
            rows.push(Row::Header("Local".to_string()));
        }
        for i in local {
            push_session(&mut rows, i);
        }
        for (peer, idxs) in remotes {
            rows.push(Row::Header(peer.to_string()));
            for i in idxs {
                push_session(&mut rows, i);
            }
        }

        // Active display-row index, and a signature over everything rendered.
        let mut active_idx = None;
        let mut sig = String::new();
        for (display_idx, row) in rows.iter().enumerate() {
            match row {
                Row::Header(label) => sig.push_str(&format!("#{label};")),
                Row::Session {
                    si,
                    name,
                    cwd,
                    panes,
                    active,
                } => {
                    if *active {
                        active_idx = Some(display_idx);
                    }
                    sig.push_str(&format!("{si}:{name}:{cwd}:{panes}:{active};"));
                }
            }
        }
        (sig, rows, active_idx)
    };
    if shell.sidebar.sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *shell.sidebar.sig.borrow_mut() = Some(sig);

    shell.sidebar.syncing.set(true);

    while let Some(child) = shell.sidebar_list.first_child() {
        shell.sidebar_list.remove(&child);
    }
    let mut targets = Vec::with_capacity(rows.len());

    for row in &rows {
        match row {
            Row::Header(label) => {
                let r = ListBoxRow::new();
                r.set_selectable(false);
                r.set_activatable(false);
                let lbl = Label::new(Some(label));
                lbl.add_css_class("dim-label");
                lbl.add_css_class("caption-heading");
                lbl.set_halign(Align::Start);
                lbl.set_margin_top(8);
                lbl.set_margin_bottom(2);
                lbl.set_margin_start(8);
                r.set_child(Some(&lbl));
                shell.sidebar_list.append(&r);
                targets.push(None);
            }
            Row::Session {
                si,
                name,
                cwd,
                panes,
                ..
            } => {
                let r = adw::ActionRow::builder().title(name).build();
                if !cwd.is_empty() {
                    r.set_subtitle(cwd);
                }
                let pill = Label::new(Some(&format!("{panes}p")));
                pill.add_css_class("dim-label");
                pill.add_css_class("caption");
                r.add_suffix(&pill);
                shell.sidebar_list.append(&r);
                targets.push(Some(*si));
            }
        }
    }
    *shell.sidebar.targets.borrow_mut() = targets;

    if let Some(i) = active_idx
        && let Some(row) = shell.sidebar_list.row_at_index(i as i32)
    {
        shell.sidebar_list.select_row(Some(&row));
    }

    shell.sidebar.syncing.set(false);
}
