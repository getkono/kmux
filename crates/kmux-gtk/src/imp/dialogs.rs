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
    Align, Box as GtkBox, Entry, EventControllerKey, Label, ListBox, ListBoxRow, Orientation,
    ScrolledWindow, SearchEntry, SelectionMode, gdk, glib,
};

use kmux_app::core::AppCore;
use kmux_app::mode::{Action, Mode};
use kmux_app::{cmd, mode};

use super::shell::Shell;
use super::{Frontend, apply_effects};

/// Which native dialog corresponds to the current mode (the list-style ones
/// share an `adw::Dialog` shape).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DialogKind {
    SessionPicker,
    ServerPicker,
    DirPicker,
    Command,
    Confirm,
    Rename,
    Help,
}

impl DialogKind {
    /// List-style dialogs (search entry + reconciled list) vs one-shot alerts.
    fn is_list(self) -> bool {
        matches!(
            self,
            DialogKind::SessionPicker
                | DialogKind::ServerPicker
                | DialogKind::DirPicker
                | DialogKind::Command
        )
    }

    fn from_mode(mode: &Mode) -> Option<Self> {
        match mode {
            Mode::SessionPicker => Some(DialogKind::SessionPicker),
            Mode::ServerPicker => Some(DialogKind::ServerPicker),
            Mode::DirectoryPicker => Some(DialogKind::DirPicker),
            Mode::Command(_) => Some(DialogKind::Command),
            Mode::ConfirmCloseSession { .. } => Some(DialogKind::Confirm),
            Mode::RenameSession { .. } | Mode::RenameTab { .. } => Some(DialogKind::Rename),
            Mode::Help => Some(DialogKind::Help),
            _ => None,
        }
    }
}

/// A live native dialog plus the handles [`sync`] updates between ticks.
struct LiveDialog {
    kind: DialogKind,
    dialog: adw::Dialog,
    list: Option<ListBox>,
}

/// The live native dialog plus the HUD OSD and the transient-state trackers.
pub struct Dialogs {
    /// Performance HUD (live OSD ticker over the grid).
    hud: GtkBox,
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

    Dialogs {
        hud,
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
    let is_list = dialogs
        .current
        .borrow()
        .as_ref()
        .is_some_and(|d| d.kind.is_list());
    if is_list {
        let sig = list_signature(&fe.borrow().core);
        if dialogs.list_sig.borrow().as_deref() != Some(sig.as_str()) {
            *dialogs.list_sig.borrow_mut() = Some(sig);
            populate_list(dialogs, fe);
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
        DialogKind::Confirm => open_confirm(shell, fe),
        DialogKind::Rename => open_rename(shell, fe),
        DialogKind::Help => open_help(shell),
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
            DialogKind::ServerPicker => (
                "Servers",
                "Filter servers",
                core.server_picker_search.clone(),
            ),
            DialogKind::DirPicker => ("Open session", "Directory…", core.dir_picker_buffer.clone()),
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
            if DialogKind::from_mode(&f.core.mode).is_some_and(|k| k.is_list()) {
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
        DialogKind::ServerPicker => {
            let rows: Vec<String> = core
                .filtered_servers()
                .iter()
                .take(50)
                .map(|s| format!("{}    {}s    {}", s.display, s.sessions.len(), s.time_ago()))
                .collect();
            let sel = core
                .server_picker_selected
                .min(rows.len().saturating_sub(1));
            (rows, sel)
        }
        DialogKind::DirPicker => {
            let rows: Vec<String> = core
                .dir_picker_matches()
                .iter()
                .take(50)
                .map(|e| {
                    let name = core.mgr.display_name_for(&e.meta.word_id);
                    format!("{name}    {}", e.meta.cwd)
                })
                .collect();
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
        Some(DialogKind::ServerPicker) => format!(
            "sv|{}|{}|{}",
            core.server_picker_search,
            core.server_picker_selected,
            core.filtered_servers().len()
        ),
        Some(DialogKind::DirPicker) => format!(
            "dir|{}|{}|{}",
            core.dir_picker_buffer,
            core.dir_picker_selected,
            core.dir_picker_matches().len()
        ),
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
        (Some(DialogKind::ServerPicker), true) => Action::ServerPickerDown,
        (Some(DialogKind::ServerPicker), false) => Action::ServerPickerUp,
        (Some(DialogKind::DirPicker), true) => Action::DirPickerDown,
        (Some(DialogKind::DirPicker), false) => Action::DirPickerUp,
        (Some(DialogKind::Command), true) => Action::CommandHintDown,
        (Some(DialogKind::Command), false) => Action::CommandHintUp,
        _ => return,
    };
    let _ = futures::executor::block_on(core.dispatch_action(action));
}

/// Activate the current selection of the open list dialog.
fn activate_current(fe: &Rc<RefCell<Frontend>>, shell: &Rc<Shell>, app: &gtk4::Application) {
    let effects = {
        let mut f = fe.borrow_mut();
        let e = match f.core.mode {
            Mode::DirectoryPicker => {
                futures::executor::block_on(f.core.dispatch_action(Action::DirPickerSubmit))
            }
            Mode::Command(_) => {
                futures::executor::block_on(f.core.dispatch_action(Action::CommandSubmit))
            }
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

fn open_confirm(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) -> LiveDialog {
    let name = {
        let core = &fe.borrow().core;
        match &core.mode {
            Mode::ConfirmCloseSession { word_id } => core.mgr.display_name_for(word_id),
            _ => String::new(),
        }
    };
    let dialog = adw::AlertDialog::new(
        Some("Close session?"),
        Some(&format!("“{name}” and all its panes will be closed.")),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("close", "Close");
    dialog.set_response_appearance("close", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    {
        let fe = fe.clone();
        let shell = shell.clone();
        dialog.connect_response(None, move |_d, resp| {
            let action = if resp == "close" {
                Action::ConfirmCloseYes
            } else {
                Action::ExitToNormal
            };
            {
                let mut f = fe.borrow_mut();
                let _ = futures::executor::block_on(f.core.dispatch_action(action));
                f.core.needs_render = true;
            }
            shell.drawing.queue_draw();
        });
    }
    dialog.present(Some(&shell.window));
    LiveDialog {
        kind: DialogKind::Confirm,
        dialog: dialog.upcast(),
        list: None,
    }
}

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
                    let _ =
                        futures::executor::block_on(f.core.dispatch_action(Action::RenameSubmit));
                } else {
                    let _ =
                        futures::executor::block_on(f.core.dispatch_action(Action::ExitToNormal));
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

/// Surface a newly-set status message as a transient toast.
fn update_toast(dialogs: &Rc<Dialogs>, shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let msg = fe.borrow().core.mgr.status_msg().to_string();
    if msg == *dialogs.last_status.borrow() {
        return;
    }
    *dialogs.last_status.borrow_mut() = msg.clone();
    if !msg.is_empty() {
        shell.toasts.add_toast(adw::Toast::new(&msg));
    }
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
    hud.set_visible(true);
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
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "-".into());
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
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".into());
    let client = info
        .client_id
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".into());
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
                .map(|v| format!("{v:.1}ms"))
                .unwrap_or_else(|| "-".into());
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
