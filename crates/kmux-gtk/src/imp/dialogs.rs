//! Native dialogs driven by `core.mode`.
//!
//! Each interactive modal is a real GTK/libadwaita widget rather than a
//! hand-drawn box: the session/server/directory pickers and the `/`-command
//! palette are `adw::Dialog`s hosting a `GtkSearchEntry` + `GtkListBox`;
//! confirm-close and rename are `adw::AlertDialog`s; help is an `adw::Dialog`.
//! `core.mode` stays the single source of truth: [`sync`] opens the dialog that
//! matches the mode, reconciles its list against the picker query/activate
//! methods, and closes it when the mode changes. Native text editing and list
//! navigation replace the per-character action path.
//!
//! Connecting/disconnected and the HUD/metrics overlays are still drawn as boxes
//! on the shell's overlay here; they become an `adw::Banner` / toast / metrics
//! dialog in a later pass.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4::{
    Align, Box as GtkBox, Button, Entry, EventControllerKey, Image, Label, ListBox, ListBoxRow,
    Orientation, ScrolledWindow, SearchEntry, SelectionMode, Spinner, gdk, glib,
};

use kmux_app::core::{AddRemoteForm, AppCore, LaunchRow, RemoteStatus};
use kmux_app::mode::{Action, Mode};
use kmux_app::{cmd, mode};

use super::shell::Shell;
use super::{Frontend, apply_effects};

/// Which native dialog corresponds to the current mode (the list-style ones
/// share an `adw::Dialog` shape).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogKind {
    SessionPicker,
    DirPicker,
    /// The unified session launcher (issue #121): a rich, hierarchical list of
    /// local + remote open/create rows from [`AppCore::launch_rows`].
    Launch,
    Command,
    Rename,
    ConfirmCloseSession,
    Help,
    /// The add-a-remote form (issue #121): a native field form, not a list.
    AddRemote,
    /// The "new session on a remote" path prompt (issue #121).
    RemoteNew,
}

impl DialogKind {
    /// List-style dialogs (search entry + reconciled list) vs one-shot alerts.
    fn is_list(self) -> bool {
        matches!(
            self,
            Self::SessionPicker | Self::DirPicker | Self::Launch | Self::Command
        )
    }

    fn from_mode(mode: &Mode) -> Option<Self> {
        match mode {
            Mode::SessionPicker => Some(Self::SessionPicker),
            Mode::DirectoryPicker => Some(Self::DirPicker),
            Mode::LaunchPicker => Some(Self::Launch),
            Mode::Command(_) => Some(Self::Command),
            Mode::RenameSession { .. } | Mode::RenameTab { .. } => Some(Self::Rename),
            Mode::ConfirmCloseSession { .. } => Some(Self::ConfirmCloseSession),
            Mode::Help => Some(Self::Help),
            Mode::AddRemote => Some(Self::AddRemote),
            Mode::RemoteNewSession { .. } => Some(Self::RemoteNew),
            _ => None,
        }
    }
}

/// A live native dialog plus the handles [`sync`] updates between ticks.
struct LiveDialog {
    kind: DialogKind,
    dialog: adw::Dialog,
    list: Option<ListBox>,
    /// The filter entry, kept so navigation (which clears the core filter) can
    /// clear the visible text too. Only the list dialogs have one.
    search: Option<SearchEntry>,
}

/// The live native dialog plus the HUD OSD and the transient-state trackers.
pub struct Dialogs {
    /// Performance HUD (live OSD ticker over the grid, top-End).
    hud: GtkBox,
    /// Render-debug overlay (top-Start): what the renderer is handed each frame.
    render_debug: GtkBox,
    current: RefCell<Option<LiveDialog>>,
    /// Content signature for the live list dialog so we only rebuild rows on a
    /// real change (not every frame of terminal output).
    list_sig: RefCell<Option<String>>,
    /// Set while we mutate the list selection programmatically, so the row's
    /// `selected` callback doesn't echo back.
    syncing: Cell<bool>,
    /// Last banner state shown, to avoid re-setting it every frame.
    banner_sig: RefCell<Option<String>>,
    /// Last status message turned into a toast.
    last_status: RefCell<String>,
    /// The metrics inspector dialog, while open.
    metrics_dialog: RefCell<Option<adw::Dialog>>,
    /// The connection inspector dialog, while open (issue #60).
    connection_dialog: RefCell<Option<adw::Dialog>>,
}

/// Build the HUD overlay and add it to the shell overlay.
pub fn build(overlay: &gtk4::Overlay) -> Dialogs {
    let hud = GtkBox::new(Orientation::Vertical, 0);
    hud.add_css_class("osd");
    hud.add_css_class("kmux-hud");
    hud.set_halign(Align::End);
    hud.set_valign(Align::Start);
    hud.set_margin_top(8);
    hud.set_margin_end(8);
    hud.set_visible(false);
    overlay.add_overlay(&hud);

    // Render-debug overlay: top-Start so it never overlaps the top-End perf HUD.
    let render_debug = GtkBox::new(Orientation::Vertical, 0);
    render_debug.add_css_class("osd");
    render_debug.add_css_class("kmux-render-debug");
    render_debug.set_halign(Align::Start);
    render_debug.set_valign(Align::Start);
    render_debug.set_margin_top(8);
    render_debug.set_margin_start(8);
    render_debug.set_visible(false);
    overlay.add_overlay(&render_debug);

    Dialogs {
        hud,
        render_debug,
        current: RefCell::new(None),
        list_sig: RefCell::new(None),
        syncing: Cell::new(false),
        banner_sig: RefCell::new(None),
        last_status: RefCell::new(String::new()),
        metrics_dialog: RefCell::new(None),
        connection_dialog: RefCell::new(None),
    }
}

/// Reconcile every dialog/overlay against `core`.
pub fn sync(
    dialogs: &Rc<Dialogs>,
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
    app: &gtk4::Application,
) {
    reconcile_native(dialogs, shell, fe, app);
    update_banner(dialogs, shell, fe);
    update_toast(dialogs, shell, fe);
    update_metrics_dialog(dialogs, shell, fe);
    update_connection_dialog(dialogs, shell, fe);
    update_hud(&dialogs.hud, &fe.borrow().core);
    update_render_debug(&dialogs.render_debug, shell, fe);
}

/// Open/close/refresh the live native dialog to match `core.mode`.
fn reconcile_native(
    dialogs: &Rc<Dialogs>,
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
    app: &gtk4::Application,
) {
    let target = DialogKind::from_mode(&fe.borrow().core.mode);
    let cur = dialogs.current.borrow().as_ref().map(|d| d.kind);

    if cur != target {
        if let Some(live) = dialogs.current.borrow_mut().take() {
            live.dialog.close();
        }
        dialogs.list_sig.borrow_mut().take();
        if let Some(kind) = target {
            let live = open_dialog(kind, dialogs, shell, fe, app);
            *dialogs.current.borrow_mut() = Some(live);
        }
    }

    // Refresh the list contents of a live picker/command dialog on change.
    let kind = dialogs.current.borrow().as_ref().map(|d| d.kind);
    if kind.is_some_and(DialogKind::is_list) {
        let sig = list_signature(&fe.borrow().core);
        if dialogs.list_sig.borrow().as_deref() != Some(sig.as_str()) {
            *dialogs.list_sig.borrow_mut() = Some(sig);
            if kind == Some(DialogKind::Launch) {
                populate_launch(dialogs, shell, fe);
            } else {
                populate_list(dialogs, fe);
            }
        }
    }
}

/// Build + present the dialog for `kind`.
fn open_dialog(
    kind: DialogKind,
    dialogs: &Rc<Dialogs>,
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
    app: &gtk4::Application,
) -> LiveDialog {
    match kind {
        DialogKind::Rename => open_rename(shell, fe),
        DialogKind::ConfirmCloseSession => open_confirm_close_session(shell, fe),
        DialogKind::Help => open_help(shell),
        DialogKind::AddRemote => open_add_remote_dialog(shell, fe),
        DialogKind::RemoteNew => open_remote_new_dialog(shell, fe),
        _ => open_list_dialog(kind, dialogs, shell, fe, app),
    }
}

/// The shared picker/command dialog: a search entry over a scrolled list.
fn open_list_dialog(
    kind: DialogKind,
    dialogs: &Rc<Dialogs>,
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
    app: &gtk4::Application,
) -> LiveDialog {
    let (title, placeholder, initial) = {
        let core = &fe.borrow().core;
        match kind {
            DialogKind::SessionPicker => (
                "Sessions",
                "Filter sessions",
                core.session_picker_search.clone(),
            ),
            DialogKind::DirPicker => (
                "New session — choose a directory",
                "Filter directories…",
                core.dir_picker_buffer.clone(),
            ),
            DialogKind::Launch => (
                "Open or create a session",
                "Filter sessions and remotes…",
                core.launch_search.clone(),
            ),
            DialogKind::Command => ("Command", "Type a command", command_buffer(core)),
            _ => unreachable!(),
        }
    };

    let search = SearchEntry::new();
    search.set_placeholder_text(Some(placeholder));
    search.set_text(&initial);
    // Keep the caret at the end so the seeded text is editable, not selected.
    search.set_position(-1);

    let list = ListBox::new();
    list.set_selection_mode(SelectionMode::Single);
    list.set_activate_on_single_click(true);
    list.add_css_class("boxed-list");

    let scroll = ScrolledWindow::new();
    scroll.set_min_content_height(280);
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));

    let content = GtkBox::new(Orientation::Vertical, 8);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);
    content.append(&search);
    content.append(&scroll);

    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(460)
        .content_height(420)
        .build();
    dialog.set_child(Some(&content));

    // Search text → core filter (the pump repopulates the list).
    {
        let fe = fe.clone();
        let shell = shell.clone();
        search.connect_search_changed(move |e| {
            let text = e.text().to_string();
            {
                let mut f = fe.borrow_mut();
                set_search(&mut f.core, text);
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }

    // Up/Down move the selection; Enter activates; Esc closes (default).
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let app = app.clone();
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(move |_c, keyval, _code, _mods| {
            let nav = match keyval {
                gdk::Key::Up => Some(false),
                gdk::Key::Down => Some(true),
                _ => None,
            };
            if let Some(down) = nav {
                let mut f = fe.borrow_mut();
                move_selection(&mut f.core, down);
                f.core.needs_render = true;
                return glib::Propagation::Stop;
            }
            if matches!(keyval, gdk::Key::Return | gdk::Key::KP_Enter) {
                activate_current(&fe, &shell, &app);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        search.add_controller(keys);
    }

    // Click a row → select + activate.
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let app = app.clone();
        let dialogs = dialogs.clone();
        list.connect_row_activated(move |_lb, row| {
            if dialogs.syncing.get() {
                return;
            }
            let idx = row.index();
            if idx < 0 {
                return;
            }
            {
                let mut f = fe.borrow_mut();
                f.core.set_picker_selected(idx as usize);
            }
            activate_current(&fe, &shell, &app);
        });
    }

    // Dismissed (Esc / click-away) → return the mode to Normal so AppCore stays
    // in sync; guard against the close we trigger ourselves on activation.
    {
        let fe = fe.clone();
        let shell = shell.clone();
        dialog.connect_closed(move |_| {
            let mut f = fe.borrow_mut();
            if DialogKind::from_mode(&f.core.mode).is_some_and(DialogKind::is_list) {
                f.core.mode = Mode::Normal;
                f.core.needs_render = true;
            }
            drop(f);
            // Return focus to the terminal so typing resumes there.
            shell.drawing.grab_focus();
            shell.drawing.queue_draw();
        });
    }

    dialog.present(Some(&shell.window));
    search.grab_focus();
    LiveDialog {
        kind,
        dialog: dialog.clone(),
        list: Some(list),
        search: Some(search),
    }
}

/// Rebuild the live list dialog's rows + selection from `core`.
fn populate_list(dialogs: &Rc<Dialogs>, fe: &Rc<RefCell<Frontend>>) {
    let cur = dialogs.current.borrow();
    let Some(live) = cur.as_ref() else {
        return;
    };
    let Some(list) = &live.list else {
        return;
    };
    let (rows, selected) = {
        let core = &fe.borrow().core;
        list_rows(live.kind, core)
    };

    dialogs.syncing.set(true);
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for label in &rows {
        let row = ListBoxRow::new();
        let lbl = Label::new(Some(label));
        lbl.set_halign(Align::Start);
        lbl.set_xalign(0.0);
        lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        lbl.set_margin_top(4);
        lbl.set_margin_bottom(4);
        lbl.set_margin_start(8);
        lbl.set_margin_end(8);
        row.set_child(Some(&lbl));
        list.append(&row);
    }
    if let Some(row) = list.row_at_index(selected as i32) {
        list.select_row(Some(&row));
    }
    dialogs.syncing.set(false);

    // Keep the directory browser's filter entry in sync with the core filter,
    // which navigation clears: when the user enters a folder we reset the filter
    // in `AppCore`, so the visible text must follow. Only touch it on a genuine
    // drift to avoid disturbing the caret while the user is typing.
    if live.kind == DialogKind::DirPicker
        && let Some(search) = &live.search
    {
        let want = fe.borrow().core.dir_picker_buffer.clone();
        if search.text() != want {
            search.set_text(&want);
            search.set_position(-1);
        }
    }
}

/// Rebuild the launcher's rich, hierarchical rows from [`AppCore::launch_rows`]
/// (issue #121). Unlike the plain-label pickers, each row is an `adw::ActionRow`
/// with an icon, a status pill / spinner, indentation for a remote's children,
/// and an inline disconnect button for a live remote. Row index maps 1:1 to
/// `launch_rows`, so the shared search/navigation/activation path drives it
/// unchanged.
fn populate_launch(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let cur = dialogs.current.borrow();
    let Some(live) = cur.as_ref() else {
        return;
    };
    let Some(list) = &live.list else {
        return;
    };
    let (rows, selected) = {
        let core = &fe.borrow().core;
        (core.launch_rows(), core.launch_selected)
    };

    dialogs.syncing.set(true);
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for row in &rows {
        list.append(&launch_row_widget(row, shell, fe));
    }
    let sel = selected.min(rows.len().saturating_sub(1));
    if let Some(r) = list.row_at_index(sel as i32) {
        list.select_row(Some(&r));
    }
    dialogs.syncing.set(false);
}

/// Build one launcher row as an activatable `adw::ActionRow`.
fn launch_row_widget(
    row: &LaunchRow,
    shell: &Rc<Shell>,
    fe: &Rc<RefCell<Frontend>>,
) -> adw::ActionRow {
    match row {
        LaunchRow::LocalNewSession { default_cwd } => {
            launch_action_row("New local session", default_cwd, "list-add-symbolic", false)
        }
        LaunchRow::LocalExisting {
            name, cwd, active, ..
        } => {
            let r = launch_action_row(name, cwd, "utilities-terminal-symbolic", false);
            if *active {
                r.add_suffix(&status_pill("active", "success", None));
            }
            r
        }
        LaunchRow::Remote {
            label,
            status,
            expanded,
            peer,
        } => {
            let chevron = if *expanded {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            };
            let r = launch_action_row(label, "", chevron, false);
            if let Some(w) = status_suffix(status) {
                r.add_suffix(&w);
            }
            if matches!(status, RemoteStatus::Connected | RemoteStatus::Connecting) {
                r.add_suffix(&disconnect_button(peer, shell, fe));
            }
            r
        }
        LaunchRow::RemoteNewSession { .. } => {
            launch_action_row("New session", "", "list-add-symbolic", true)
        }
        LaunchRow::RemoteExisting {
            name, cwd, active, ..
        } => {
            let r = launch_action_row(name, cwd, "utilities-terminal-symbolic", true);
            if *active {
                r.add_suffix(&status_pill("active", "success", None));
            }
            r
        }
        LaunchRow::ClosedSession {
            name,
            cwd,
            last_active_ms,
            ..
        } => {
            let when = kmux_app::core::relative_time_label(*last_active_ms);
            let subtitle = if cwd.is_empty() {
                when.clone()
            } else {
                format!("{cwd} · {when}")
            };
            let r = launch_action_row(name, &subtitle, "view-refresh-symbolic", false);
            r.add_suffix(&status_pill(
                "restore",
                "accent",
                Some("Restore this closed session"),
            ));
            r
        }
        LaunchRow::AddRemote => {
            launch_action_row("Add remote…", "", "network-server-symbolic", false)
        }
    }
}

/// A launcher `adw::ActionRow`: a leading icon, optional subtitle, and a left
/// spacer when it is a remote's (indented) child row.
fn launch_action_row(title: &str, subtitle: &str, icon: &str, indent: bool) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(title).build();
    r.set_activatable(true);
    if !subtitle.is_empty() {
        r.set_subtitle(subtitle);
    }
    if indent {
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_size_request(20, -1);
        r.add_prefix(&spacer);
    }
    r.add_prefix(&Image::from_icon_name(icon));
    r
}

/// A small caption pill (e.g. "active"/"connected") in a semantic color, with an
/// optional tooltip carrying the detail (used for an error reason).
fn status_pill(text: &str, css: &str, tip: Option<&str>) -> Label {
    let l = Label::new(Some(text));
    l.add_css_class(css);
    l.add_css_class("caption");
    l.set_valign(Align::Center);
    if let Some(t) = tip {
        l.set_tooltip_text(Some(t));
    }
    l
}

/// The trailing status indicator for a remote row: a spinner while connecting, a
/// colored pill when connected/errored, nothing when idle.
fn status_suffix(status: &RemoteStatus) -> Option<gtk4::Widget> {
    match status {
        RemoteStatus::Idle => None,
        RemoteStatus::Connecting => {
            let s = Spinner::new();
            s.start();
            s.set_valign(Align::Center);
            s.set_tooltip_text(Some("Connecting…"));
            Some(s.upcast())
        }
        RemoteStatus::Connected => Some(status_pill("connected", "success", None).upcast()),
        RemoteStatus::Error(e) => Some(status_pill("error", "error", Some(e)).upcast()),
    }
}

/// An inline "disconnect this remote" button (issue #121). Clicking it consumes
/// the press, so it does not also toggle the row's expand state.
fn disconnect_button(peer: &str, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> Button {
    let btn = Button::from_icon_name("network-offline-symbolic");
    btn.set_tooltip_text(Some("Disconnect"));
    btn.add_css_class("flat");
    btn.set_valign(Align::Center);
    let peer = peer.to_string();
    let fe = fe.clone();
    let shell = shell.clone();
    btn.connect_clicked(move |_| {
        {
            let mut f = fe.borrow_mut();
            f.core.disconnect_remote(&peer);
            f.core.needs_render = true;
        }
        shell.drawing.queue_draw();
    });
    btn
}

/// Row labels + selected index for a list dialog.
fn list_rows(kind: DialogKind, core: &AppCore) -> (Vec<String>, usize) {
    match kind {
        DialogKind::SessionPicker => {
            let mut rows = vec!["＋  New session…".to_string()];
            for e in core.session_picker_matches().iter().take(50) {
                let name = core.mgr.display_name_for(&e.meta.word_id);
                rows.push(format!("{name}    {}p    {}", e.panes.len(), e.meta.cwd));
            }
            let sel = core
                .session_picker_selected
                .min(rows.len().saturating_sub(1));
            (rows, sel)
        }
        DialogKind::DirPicker => {
            use kmux_app::core::DirBrowserRow;
            let mut rows: Vec<String> = core
                .dir_browser_rows()
                .into_iter()
                .map(|row| match row {
                    DirBrowserRow::CreateHere { cwd } => format!("＋  New session in {cwd}"),
                    DirBrowserRow::Up { parent } => format!("..  {parent}"),
                    DirBrowserRow::Enter { name, .. } => format!("📁  {name}"),
                })
                .collect();
            // Surface a listing error as a trailing dim-ish row (plain label here;
            // the list uses a single style, so we just make it readable text).
            if let Some(err) = core.dir_browser_error() {
                rows.push(format!("⚠  {err}"));
            }
            let sel = core.dir_picker_selected.min(rows.len().saturating_sub(1));
            (rows, sel)
        }
        DialogKind::Command => {
            let hints = cmd::hint::build_hints(core);
            let sel = command_selected(core).min(hints.len().saturating_sub(1));
            let rows = hints
                .iter()
                .map(|h| format!("{}    {}", h.display.trim_end(), h.summary))
                .collect();
            (rows, sel)
        }
        _ => (Vec::new(), 0),
    }
}

/// Signature of the list dialog's contents (mode buffer + selection + matches).
fn list_signature(core: &AppCore) -> String {
    match DialogKind::from_mode(&core.mode) {
        Some(DialogKind::SessionPicker) => format!(
            "se|{}|{}|{}",
            core.session_picker_search,
            core.session_picker_selected,
            core.session_picker_matches().len()
        ),
        Some(DialogKind::DirPicker) => format!(
            "dir|{}|{}|{}|{}|{}",
            core.dir_browser_cwd,
            core.dir_picker_buffer,
            core.dir_picker_selected,
            core.dir_browser_rows().len(),
            core.dir_browser_error().unwrap_or("")
        ),
        Some(DialogKind::Launch) => {
            // Captures every field that changes a row's look (expand, status,
            // active, peer set) so the launcher rebuilds only on real change.
            let body: String = core
                .launch_rows()
                .iter()
                .map(|r| format!("{r:?};"))
                .collect();
            format!("la|{}|{}|{body}", core.launch_search, core.launch_selected)
        }
        Some(DialogKind::Command) => {
            format!("cmd|{}|{}", command_buffer(core), command_selected(core))
        }
        _ => String::new(),
    }
}

/// Set the active filter text for the open list dialog.
fn set_search(core: &mut AppCore, text: String) {
    if matches!(core.mode, Mode::Command(_)) {
        set_command_buffer(core, text);
    } else {
        core.set_picker_search(text);
    }
}

/// Move the selection in the open list dialog (true = down).
fn move_selection(core: &mut AppCore, down: bool) {
    let action = match (DialogKind::from_mode(&core.mode), down) {
        (Some(DialogKind::SessionPicker), true) => Action::PickerDown,
        (Some(DialogKind::SessionPicker), false) => Action::PickerUp,
        (Some(DialogKind::DirPicker), true) => Action::DirPickerDown,
        (Some(DialogKind::DirPicker), false) => Action::DirPickerUp,
        (Some(DialogKind::Launch), true) => Action::LaunchDown,
        (Some(DialogKind::Launch), false) => Action::LaunchUp,
        (Some(DialogKind::Command), true) => Action::CommandHintDown,
        (Some(DialogKind::Command), false) => Action::CommandHintUp,
        _ => return,
    };
    let _ = core.dispatch_action(action);
}

/// Activate the current selection of the open list dialog.
fn activate_current(fe: &Rc<RefCell<Frontend>>, shell: &Rc<Shell>, app: &gtk4::Application) {
    let effects = {
        let mut f = fe.borrow_mut();
        let e = match f.core.mode {
            Mode::DirectoryPicker => f.core.dispatch_action(Action::DirPickerSubmit),
            Mode::Command(_) => f.core.dispatch_action(Action::CommandSubmit),
            _ => f.core.activate_picker_selection(),
        };
        f.core.needs_render = true;
        e
    };
    apply_effects(fe, effects, app, &shell.drawing);
    shell.drawing.queue_draw();
}

// ── Command-buffer helpers (the buffer lives in Mode::Command(state)) ──

fn command_buffer(core: &AppCore) -> String {
    match &core.mode {
        Mode::Command(s) => s.buffer.clone(),
        _ => String::new(),
    }
}

fn command_selected(core: &AppCore) -> usize {
    match &core.mode {
        Mode::Command(s) => s.selected,
        _ => 0,
    }
}

/// Replace the command buffer from a native entry (no per-char actions).
fn set_command_buffer(core: &mut AppCore, text: String) {
    if let Mode::Command(s) = &mut core.mode {
        s.cursor = text.len();
        s.buffer = text;
        s.selected = 0;
        s.history_pos = None;
    }
}

// ── One-shot alert dialogs ──

fn open_rename(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> LiveDialog {
    let (current, title) = {
        let core = &fe.borrow().core;
        match &core.mode {
            Mode::RenameSession { buffer, .. } => (buffer.clone(), "Rename session"),
            Mode::RenameTab { buffer, .. } => (buffer.clone(), "Rename tab"),
            _ => (String::new(), "Rename"),
        }
    };
    let entry = Entry::new();
    entry.set_text(&current);
    entry.set_activates_default(true);

    let dialog = adw::AlertDialog::new(Some(title), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("rename", "Rename");
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let entry = entry.clone();
        dialog.connect_response(None, move |_d, resp| {
            {
                let mut f = fe.borrow_mut();
                if resp == "rename" {
                    if let Mode::RenameSession { buffer, .. } | Mode::RenameTab { buffer, .. } =
                        &mut f.core.mode
                    {
                        *buffer = entry.text().to_string();
                    }
                    let _ = f.core.dispatch_action(Action::RenameSubmit);
                } else {
                    let _ = f.core.dispatch_action(Action::ExitToNormal);
                }
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }
    dialog.present(Some(&shell.window));
    LiveDialog {
        kind: DialogKind::Rename,
        dialog: dialog.upcast(),
        list: None,
        search: None,
    }
}

fn open_confirm_close_session(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> LiveDialog {
    let name = {
        let core = &fe.borrow().core;
        match &core.mode {
            Mode::ConfirmCloseSession { name, .. } => name.clone(),
            _ => String::new(),
        }
    };
    let body = if name.is_empty() {
        "This will close the session and all of its tabs and panes.".to_string()
    } else {
        format!("Close session “{name}” and all of its tabs and panes?")
    };
    let dialog = adw::AlertDialog::new(Some("Close session?"), Some(&body));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("close", "Close Session");
    dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    {
        let fe = fe.clone();
        let shell = shell.clone();
        dialog.connect_response(None, move |_d, resp| {
            {
                let mut f = fe.borrow_mut();
                let action = if resp == "close" {
                    Action::ConfirmCloseSession
                } else {
                    Action::ExitToNormal
                };
                let _ = f.core.dispatch_action(action);
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }
    dialog.present(Some(&shell.window));
    LiveDialog {
        kind: DialogKind::ConfirmCloseSession,
        dialog: dialog.upcast(),
        list: None,
        search: None,
    }
}

fn open_help(shell: &Rc<Shell>) -> LiveDialog {
    let text: String = mode::help_entries()
        .iter()
        .map(|(k, d)| {
            if d.is_empty() {
                format!("{k}\n")
            } else {
                format!("  {k:<16}{d}\n")
            }
        })
        .collect();
    let body = Label::new(Some(text.trim_end()));
    body.set_halign(Align::Start);
    body.set_valign(Align::Start);
    body.set_xalign(0.0);
    body.add_css_class("monospace");
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&body));

    let dialog = adw::Dialog::builder()
        .title("Help")
        .content_width(560)
        .content_height(520)
        .build();
    dialog.set_child(Some(&scroll));
    dialog.present(Some(&shell.window));
    LiveDialog {
        kind: DialogKind::Help,
        dialog: dialog.upcast(),
        list: None,
        search: None,
    }
}

// ── Add-remote + remote-new-session forms (issue #121) ──

/// The add-a-remote form: a native field form (kind, host, user, port, token,
/// accept-invalid-certs) submitted to [`AppCore::submit_add_remote`]. Unlike the
/// list dialogs this is a plain `adw::Dialog` so we control validation — a bad
/// form shows an inline error and stays open; a good one connects on focus and
/// returns to the launcher with the new remote expanded.
fn open_add_remote_dialog(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> LiveDialog {
    let host = adw::EntryRow::builder().title("Host").build();
    let user = adw::EntryRow::builder().title("User (optional)").build();
    let port = adw::EntryRow::builder().title("Port (optional)").build();
    let certs = adw::SwitchRow::builder()
        .title("Accept invalid certificates")
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&host);
    group.add(&user);
    group.add(&port);
    group.add(&certs);

    let error = Label::new(None);
    error.add_css_class("error");
    error.set_halign(Align::Start);
    error.set_wrap(true);
    error.set_visible(false);

    let body = GtkBox::new(Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.append(&group);
    body.append(&error);

    let cancel = Button::with_label("Cancel");
    let add = Button::with_label("Add");
    add.add_css_class("suggested-action");

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Add remote", "")));
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.pack_start(&cancel);
    header.pack_end(&add);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    let dialog = adw::Dialog::builder()
        .title("Add remote")
        .content_width(460)
        .build();
    dialog.set_child(Some(&toolbar));

    // Cancel → return to Normal and close.
    {
        let fe = fe.clone();
        let dialog2 = dialog.clone();
        cancel.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                let _ = f.core.dispatch_action(Action::LaunchOverlayCancel);
                f.core.needs_render = true;
            }
            dialog2.close();
        });
    }

    // Add → validate via the core; on error show it inline and stay open, on
    // success reopen the launcher (the remote is now expanded + connecting).
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let host = host.clone();
        let user = user.clone();
        let port = port.clone();
        let certs = certs.clone();
        let error = error.clone();
        add.connect_clicked(move |_| {
            let form = AddRemoteForm {
                host: host.text().to_string(),
                user: user.text().to_string(),
                port: port.text().trim().parse::<u16>().ok(),
                accept_invalid_certs: certs.is_active(),
            };
            let result = {
                let mut f = fe.borrow_mut();
                let r = f.core.submit_add_remote(form);
                if r.is_ok() {
                    // Core returns to Normal on success; reopen the launcher so the
                    // new remote is visible (expanded, connecting on focus).
                    f.core.open_launch_picker();
                }
                f.core.needs_render = true;
                r
            };
            match result {
                Ok(()) => shell.drawing.queue_draw(),
                Err(e) => {
                    error.set_text(&e);
                    error.set_visible(true);
                }
            }
        });
    }

    // Esc / click-away → treat as cancel if still in the form.
    {
        let fe = fe.clone();
        let shell = shell.clone();
        dialog.connect_closed(move |_| {
            let mut f = fe.borrow_mut();
            if matches!(f.core.mode, Mode::AddRemote) {
                f.core.mode = Mode::Normal;
                f.core.needs_render = true;
            }
            drop(f);
            shell.drawing.grab_focus();
            shell.drawing.queue_draw();
        });
    }

    dialog.present(Some(&shell.window));
    host.grab_focus();
    LiveDialog {
        kind: DialogKind::AddRemote,
        dialog: dialog.upcast(),
        list: None,
        search: None,
    }
}

/// The "new session on a remote" path prompt: a one-field alert seeded from that
/// peer's focused session cwd, submitted to [`AppCore::submit_remote_new_session`].
fn open_remote_new_dialog(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> LiveDialog {
    let peer = match &fe.borrow().core.mode {
        Mode::RemoteNewSession { peer } => peer.clone(),
        _ => String::new(),
    };
    let seed = {
        let core = &fe.borrow().core;
        let active = core.mgr.active_session();
        let on_peer =
            |e: &&kmux_protocol::messages::SessionEntry| e.peer.as_deref() == Some(peer.as_str());
        core.mgr
            .session_list()
            .iter()
            .find(|e| on_peer(e) && active == Some(e.meta.word_id.as_str()))
            .or_else(|| core.mgr.session_list().iter().find(on_peer))
            .map(|e| e.meta.cwd.clone())
            .unwrap_or_default()
    };

    let entry = Entry::new();
    entry.set_text(&seed);
    entry.set_placeholder_text(Some("Path (blank = remote default)"));
    entry.set_activates_default(true);

    let dialog = adw::AlertDialog::new(
        Some("New remote session"),
        Some(&format!("Create a session on “{peer}”.")),
    );
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let entry = entry.clone();
        dialog.connect_response(None, move |_d, resp| {
            {
                let mut f = fe.borrow_mut();
                if resp == "create" {
                    let cwd = entry.text().to_string();
                    f.core.submit_remote_new_session(peer.clone(), cwd);
                } else {
                    let _ = f.core.dispatch_action(Action::LaunchOverlayCancel);
                }
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }
    dialog.present(Some(&shell.window));
    LiveDialog {
        kind: DialogKind::RemoteNew,
        dialog: dialog.upcast(),
        list: None,
        search: None,
    }
}

fn clear(b: &GtkBox) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

// ── Connection banner + status toasts ──

/// Drive the connecting/disconnected banner from the connection mode. The
/// banner's button (reconnect) is wired once in `main::build_ui`.
fn update_banner(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, title, button, revealed) = {
        let core = &fe.borrow().core;
        match &core.mode {
            Mode::Connecting { target_display } => (
                format!("c|{target_display}"),
                format!("Connecting to {target_display}…"),
                None,
                true,
            ),
            Mode::Disconnected { reason } => (
                format!("d|{reason}"),
                format!("Disconnected — {reason}"),
                Some("Reconnect"),
                true,
            ),
            _ => ("n".to_string(), String::new(), None, false),
        }
    };
    if dialogs.banner_sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *dialogs.banner_sig.borrow_mut() = Some(sig);
    shell.banner.set_title(&title);
    shell.banner.set_button_label(button);
    shell.banner.set_revealed(revealed);
}

/// Surface a newly-set status message as a transient toast. While a pane is in
/// its soft-close grace window (issue #86) the toast gains an "Undo" button that
/// cancels the pending close.
fn update_toast(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (msg, pending) = {
        let core = &fe.borrow().core;
        (core.mgr.status_msg().to_string(), core.has_pending_close())
    };
    if msg == *dialogs.last_status.borrow() {
        return;
    }
    *dialogs.last_status.borrow_mut() = msg.clone();
    if msg.is_empty() {
        return;
    }
    let toast = adw::Toast::new(&msg);
    if pending {
        toast.set_button_label(Some("Undo"));
        let fe = fe.clone();
        let shell = shell.clone();
        toast.connect_button_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                let _ = f.core.dispatch_action(Action::UndoClose);
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }
    shell.toasts.add_toast(toast);
}

// ── HUD ticker + metrics inspector dialog ──

fn update_hud(hud: &GtkBox, core: &AppCore) {
    if !core.hud_visible {
        hud.set_visible(false);
        return;
    }
    clear(hud);
    let snap = core.mgr.metrics.snapshot(core.force_snapshot_mode);
    let c = &snap.counters;
    let lines = [
        format!(
            "Net+Apply: {:.1}ms avg / {:.1}ms max",
            snap.net_apply_avg_ms, snap.net_apply_max_ms
        ),
        format!("Apply:     {:.2}ms avg", snap.apply_avg_ms),
        format!("Batch:     {:.1} msgs avg", snap.batch_avg),
        format!("Diff:      {} ops", snap.last_diff_ops),
        format!("LargeDiff: {:.1}ms", snap.last_large_diff_ms),
        format!(
            "Snapshot:  {}",
            if snap.snapshot_mode { "FORCED" } else { "off" }
        ),
        format!(
            "Disc:{} Gap:{} Lag:{} Sync:{} Tear:{}",
            c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs, c.tears
        ),
    ];
    for line in lines {
        hud.append(&label(&line, "kmux-hud-line"));
    }
    // Network latency + rendering FPS (issue #61), shown unless disabled in
    // config (which also skips their computation). A ★ marks a stale link
    // (no inbound for >3× the ping interval).
    if core.show_perf_counters {
        let latency = match core.net_latency_ms() {
            Some(ms) => format!("{ms:.1} ms"),
            None => "—".to_string(),
        };
        let star = if core.net_latency_stale() { " ★" } else { "" };
        hud.append(&label(
            &format!("Latency:   {latency}{star}"),
            "kmux-hud-line",
        ));
        hud.append(&label(
            &format!("FPS:       {}", core.render_fps()),
            "kmux-hud-line",
        ));
    }
    hud.set_visible(true);
}

/// Render-debug overlay (top-Start): what the renderer is handed each frame —
/// the active renderer leaf, frame/grid/cell geometry, the cursor's logical
/// state, and the exact pixel rect `kmux_render::cursor_geometry` computes for it
/// (compare against what the active path actually draws — e.g. the Cairo path's
/// hardcoded 2px cursor vs the renderer's scale-aware `cursor_thickness`). Scene
/// primitive counts appear on the GPU path.
fn update_render_debug(overlay: &GtkBox, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let f = fe.borrow();
    if !f.core.render_debug_visible() {
        overlay.set_visible(false);
        return;
    }
    clear(overlay);

    let frame_w = shell.drawing.width().max(0) as u32;
    let frame_h = shell.drawing.height().max(0) as u32;
    let scale = shell.drawing.scale_factor() as f32;

    // The *effective* renderer (live state), not the configured one: a GPU-init
    // failure falls back to Cairo, and the overlay must reflect what is actually
    // drawing this frame.
    #[cfg(feature = "gpu")]
    let renderer = f.gpu.active_renderer_name();
    #[cfg(not(feature = "gpu"))]
    let renderer = "cairo";

    let snap = f
        .core
        .render_debug_snapshot(frame_w, frame_h, scale, renderer);
    // The renderer's own cell geometry (so the pixel rect matches kmux-render).
    let cell = kmux_render::CellMetrics::new(f.metrics.cell_w as f32, f.metrics.cell_h as f32);

    let mut lines: Vec<String> = vec![
        format!(
            "renderer: {}   frame: {}×{}px   scale: {}",
            snap.renderer, snap.frame_width, snap.frame_height, snap.scale
        ),
        format!(
            "cell: {:.1}×{:.1}px   cursor_thickness: {:.2}px   blink_on: {}",
            cell.cell_w, cell.cell_h, cell.cursor_thickness, snap.blink_on
        ),
    ];

    match &snap.pane {
        None => lines.push("pane: (none)".to_string()),
        Some(p) => {
            lines.push(format!(
                "pane: {}   grid: {}×{}   scroll: {}",
                p.pane_id, p.grid_cols, p.grid_rows, p.scroll_offset
            ));
            match &p.cursor {
                None => lines.push("cursor: hidden (scrolled into history)".to_string()),
                Some(c) => {
                    lines.push(format!(
                        "cursor: ({},{}) {:?}  blink={} visible={} drawn={}",
                        c.col, c.row, c.shape, c.blink, c.visible, c.is_drawn
                    ));
                    // Pane-relative pixel rect the renderer would fill.
                    let cv = kmux_render::CursorView {
                        col: c.col,
                        row: c.row,
                        shape: c.shape,
                        blink: c.blink,
                        visible: c.visible,
                    };
                    let geo = kmux_render::cursor_geometry(
                        &cv,
                        (0.0, 0.0),
                        p.grid_cols,
                        p.grid_rows,
                        &cell,
                    );
                    if !geo.in_range {
                        lines.push("  px: out of range".to_string());
                    } else if let Some(r) = geo.rects.first() {
                        lines.push(format!(
                            "  px: x={:.1} y={:.1} w={:.1} h={:.1}  ({} rect/s)",
                            r.x,
                            r.y,
                            r.w,
                            r.h,
                            geo.rects.len()
                        ));
                    } else {
                        lines.push("  px: no rects (hidden shape)".to_string());
                    }
                }
            }
        }
    }

    #[cfg(feature = "gpu")]
    if let Some(counts) = f.gpu.last_scene_counts() {
        lines.push(format!(
            "scene: bg={} glyphs={} overlay={} ov-glyphs={}",
            counts.bg_quads, counts.glyphs, counts.overlay_quads, counts.overlay_glyphs
        ));
    }

    for line in lines {
        overlay.append(&label(&line, "kmux-render-debug-line"));
    }
    overlay.set_visible(true);
}

/// Open/close the metrics inspector dialog from `metrics_overlay_visible`. The
/// content is a snapshot at open time (the HUD is the live ticker); closing the
/// dialog clears the flag.
fn update_metrics_dialog(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let show = fe.borrow().core.metrics_overlay_visible;
    let open = dialogs.metrics_dialog.borrow().is_some();
    if show && !open {
        let dialog = adw::Dialog::builder()
            .title("Metrics")
            .content_width(560)
            .content_height(440)
            .build();
        dialog.set_child(Some(&metrics_content(&fe.borrow().core)));
        {
            let fe = fe.clone();
            let shell = shell.clone();
            dialog.connect_closed(move |_| {
                fe.borrow_mut().core.metrics_overlay_visible = false;
                shell.drawing.grab_focus();
            });
        }
        dialog.present(Some(&shell.window));
        *dialogs.metrics_dialog.borrow_mut() = Some(dialog);
    } else if !show
        && open
        && let Some(d) = dialogs.metrics_dialog.borrow_mut().take()
    {
        d.close();
    }
}

/// The metrics inspector body: connection identity, per-transport traffic, and
/// the render summary, in a scrolled list using stock GTK style classes.
fn metrics_content(core: &AppCore) -> ScrolledWindow {
    let card = GtkBox::new(Orientation::Vertical, 4);
    card.set_margin_top(12);
    card.set_margin_bottom(12);
    card.set_margin_start(12);
    card.set_margin_end(12);
    let mgr = &core.mgr;
    let metrics = &mgr.metrics;

    let conn = mgr
        .connection_id
        .map_or_else(|| "-".into(), |c| c.0.to_string());
    card.append(&label(
        &format!("pid {}   connection {conn}", std::process::id()),
        "dim-label",
    ));
    let sink = match metrics.sink_path() {
        Some(p) => format!("sink: {}", p.display()),
        None => "sink: (disabled)".to_string(),
    };
    card.append(&label(&sink, "dim-label"));

    let by_transport = metrics.network.snapshot_by_transport();
    if by_transport.is_empty() {
        card.append(&label("(no transport traffic yet)", "dim-label"));
    } else {
        for (key, totals) in &by_transport {
            card.append(&label(&format!("{} {}", key.kind, key.address), "heading"));
            card.append(&label(
                &format!(
                    "   in {}  out {}   msgs {}/{}",
                    fmt_bytes(totals.bytes_in),
                    fmt_bytes(totals.bytes_out),
                    totals.msgs_in,
                    totals.msgs_out,
                ),
                "monospace",
            ));
        }
    }

    let snap = metrics.snapshot(core.force_snapshot_mode);
    let c = &snap.counters;
    card.append(&label(
        &format!(
            "Render: net+apply {:.1}/{:.1}ms  apply {:.2}ms  batch {:.1}",
            snap.net_apply_avg_ms, snap.net_apply_max_ms, snap.apply_avg_ms, snap.batch_avg
        ),
        "monospace",
    ));
    card.append(&label(
        &format!(
            "Disc:{} Gap:{} Lag:{} Sync:{} Tear:{}",
            c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs, c.tears
        ),
        "monospace",
    ));

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&card));
    scroll
}

// ── Connection inspector dialog (issue #60) ──

/// Open/close the connection inspector dialog from `connection_overlay_visible`,
/// mirroring [`update_metrics_dialog`]. The content is rebuilt each open from a
/// live [`kmux_app::core::ConnectionInfo`]; closing the dialog clears the flag.
fn update_connection_dialog(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let show = fe.borrow().core.connection_overlay_visible;
    let open = dialogs.connection_dialog.borrow().is_some();
    if show && !open {
        let dialog = adw::Dialog::builder()
            .title("Connection")
            .content_width(560)
            .content_height(460)
            .build();
        dialog.set_child(Some(&connection_content(&fe.borrow().core)));
        {
            let fe = fe.clone();
            let shell = shell.clone();
            dialog.connect_closed(move |_| {
                fe.borrow_mut().core.connection_overlay_visible = false;
                shell.drawing.grab_focus();
            });
        }
        dialog.present(Some(&shell.window));
        *dialogs.connection_dialog.borrow_mut() = Some(dialog);
    } else if !show
        && open
        && let Some(d) = dialogs.connection_dialog.borrow_mut().take()
    {
        d.close();
    }
}

/// The connection inspector body: server/endpoint, transport + state, the
/// session/handshake identity, the live latency summary, and per-transport
/// traffic — all from the toolkit-neutral [`kmux_app::core::ConnectionInfo`].
fn connection_content(core: &AppCore) -> ScrolledWindow {
    let info = core.connection_info();
    let card = GtkBox::new(Orientation::Vertical, 4);
    card.set_margin_top(12);
    card.set_margin_bottom(12);
    card.set_margin_start(12);
    card.set_margin_end(12);

    card.append(&label("Server", "heading"));
    card.append(&label(&info.server, "monospace"));
    if !info.is_local && !info.endpoint.is_empty() {
        card.append(&label(
            &format!("   endpoint {}", info.endpoint),
            "dim-label",
        ));
    }
    card.append(&label(&format!("State: {}", info.state), "monospace"));
    card.append(&label(
        &format!("Transport: {}", info.transport),
        "monospace",
    ));
    if !info.is_local {
        let tls = if info.accept_invalid_certs {
            "TLS: accepting invalid certs (dev)"
        } else {
            "TLS: certificate verified"
        };
        card.append(&label(tls, "dim-label"));
    }

    card.append(&label("Identity", "heading"));
    let conn = info
        .connection_id
        .map_or_else(|| "-".into(), |c| c.to_string());
    let client = info.client_id.map_or_else(|| "-".into(), |c| c.to_string());
    card.append(&label(
        &format!("connection {conn}   client {client}"),
        "monospace",
    ));
    let server_ver = info.server_version.as_deref().unwrap_or("unknown");
    card.append(&label(
        &format!("server v{server_ver}   protocol v{}", info.protocol_version),
        "monospace",
    ));

    card.append(&label("Latency", "heading"));
    match &info.rtt {
        Some(rtt) => {
            let ewma = rtt
                .ewma_ms
                .map_or_else(|| "-".into(), |v| format!("{v:.1}ms"));
            card.append(&label(
                &format!(
                    "ping {ewma} ewma   recent {:.1}/{:.1}ms   {} samples",
                    rtt.recent_avg_ms, rtt.recent_max_ms, rtt.samples
                ),
                "monospace",
            ));
        }
        None => card.append(&label("(no ping samples yet)", "dim-label")),
    }

    card.append(&label("Traffic", "heading"));
    if info.transports.is_empty() {
        card.append(&label("(no transport traffic yet)", "dim-label"));
    } else {
        for t in &info.transports {
            card.append(&label(
                &format!(
                    "{}   in {}  out {}   msgs {}/{}",
                    t.label,
                    fmt_bytes(t.bytes_in),
                    fmt_bytes(t.bytes_out),
                    t.msgs_in,
                    t.msgs_out,
                ),
                "monospace",
            ));
        }
    }

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&card));
    scroll
}

fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n:.0}B")
    } else if n < K * K {
        format!("{:.1}KB", n / K)
    } else if n < K * K * K {
        format!("{:.1}MB", n / (K * K))
    } else {
        format!("{:.1}GB", n / (K * K * K))
    }
}

fn label(text: &str, css: &str) -> Label {
    let l = Label::new(Some(text));
    l.add_css_class(css);
    l.set_halign(Align::Start);
    l
}
