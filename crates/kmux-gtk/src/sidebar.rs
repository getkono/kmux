//! Sessions sidebar: a `GtkListBox` of the live sessions, reconciled against
//! `mgr.session_list()`. Selecting a row jumps to that session; the header
//! "＋" button creates a new one. Optimized for rapid swapping (always-visible
//! one-click targets; `Ctrl+1..9` / `Ctrl+Tab` accelerators live in `actions`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{Application, Label};

use kmux_app::mode::Action;

use crate::Frontend;
use crate::shell::Shell;

/// Row→session mapping (row index == `session_list` index), a re-entrancy guard
/// so programmatic selection doesn't echo back as a jump, and a signature.
pub struct SidebarState {
    ids: RefCell<Vec<String>>,
    syncing: Cell<bool>,
    sig: RefCell<Option<String>>,
}

impl SidebarState {
    pub fn new() -> Self {
        Self {
            ids: RefCell::new(Vec::new()),
            syncing: Cell::new(false),
            sig: RefCell::new(None),
        }
    }
}

/// Wire the sidebar interactions. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, _app: &Application) {
    // "＋ New session": create one in the initial cwd.
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.new_session_btn.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                let _ = futures::executor::block_on(f.core.dispatch_action(Action::CreateSession));
                f.core.needs_render = true;
            }
            s.drawing.queue_draw();
        });
    }

    // Row selected → jump to that session (row index == session_list index).
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
            {
                let mut f = fe.borrow_mut();
                let _ = futures::executor::block_on(
                    f.core.dispatch_action(Action::JumpToSession(idx as usize)),
                );
                f.core.needs_render = true;
            }
            // Return focus to the terminal so typing goes to the session.
            s.drawing.grab_focus();
            s.drawing.queue_draw();
        });
    }
}

/// Reconcile the session rows. Cheap no-op when the session set + active session
/// are unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, rows, active_idx) = {
        let f = fe.borrow();
        let mgr = &f.core.mgr;
        let active = mgr.active_session();
        let mut rows: Vec<(String, String, String, usize)> = Vec::new();
        let mut active_idx = None;
        for (i, e) in mgr.session_list().iter().enumerate() {
            let name = mgr.display_name_for(&e.meta.word_id);
            if active == Some(e.meta.word_id.as_str()) {
                active_idx = Some(i);
            }
            rows.push((
                e.meta.word_id.clone(),
                name,
                e.meta.cwd.clone(),
                e.panes.len(),
            ));
        }
        let sig = format!(
            "{active:?}|{}",
            rows.iter()
                .map(|(id, n, c, p)| format!("{id}:{n}:{c}:{p};"))
                .collect::<String>()
        );
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
    shell.sidebar.ids.borrow_mut().clear();

    for (id, name, cwd, panes) in &rows {
        let row = adw::ActionRow::builder().title(name).build();
        if !cwd.is_empty() {
            row.set_subtitle(cwd);
        }
        let pill = Label::new(Some(&format!("{panes}p")));
        pill.add_css_class("dim-label");
        pill.add_css_class("caption");
        row.add_suffix(&pill);
        shell.sidebar_list.append(&row);
        shell.sidebar.ids.borrow_mut().push(id.clone());
    }

    if let Some(i) = active_idx
        && let Some(row) = shell.sidebar_list.row_at_index(i as i32)
    {
        shell.sidebar_list.select_row(Some(&row));
    }

    shell.sidebar.syncing.set(false);
}
