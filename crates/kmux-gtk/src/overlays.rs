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
    Orientation, Overlay, ScrolledWindow,
};

use kmux_app::cmd;
use kmux_app::core::{AppCore, KeyResult};
use kmux_app::mode::{self, Action, Mode};

use crate::{Frontend, handle_effect};

/// Persistent overlay widgets plus the last-rendered modal signature.
pub struct Overlays {
    /// Centered modal card for the mode-driven overlays (pickers, command,
    /// help, confirm, rename, connecting, disconnected).
    modal: GtkBox,
    /// Top-right HUD with live render metrics (toggled by `hud_visible`).
    hud: GtkBox,
    /// Centered card with the full metrics breakdown (`metrics_overlay_visible`).
    metrics: GtkBox,
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

    let hud = GtkBox::new(Orientation::Vertical, 0);
    hud.add_css_class("kmux-overlay");
    hud.add_css_class("kmux-hud");
    hud.set_halign(Align::End);
    hud.set_valign(Align::Start);
    hud.set_visible(false);
    overlay.add_overlay(&hud);

    let metrics = GtkBox::new(Orientation::Vertical, 0);
    metrics.add_css_class("kmux-overlay");
    metrics.set_halign(Align::Center);
    metrics.set_valign(Align::Center);
    metrics.set_visible(false);
    overlay.add_overlay(&metrics);

    Overlays {
        modal,
        hud,
        metrics,
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

/// Sync all overlays. The mode-driven modal is rebuilt only when its signature
/// changes; the HUD and metrics card carry live data, so they rebuild on every
/// call (the pump forces a tick while either is visible).
pub fn sync(ov: &Overlays, fe: &Rc<RefCell<Frontend>>, app: &Application, drawing: &DrawingArea) {
    let sig = signature(&fe.borrow().core);
    if ov.last_sig.borrow().as_deref() != Some(sig.as_str()) {
        *ov.last_sig.borrow_mut() = Some(sig);
        rebuild_modal(ov, fe, app, drawing);
    }

    let f = fe.borrow();
    update_hud(&ov.hud, &f.core);
    update_metrics(&ov.metrics, &f.core);
}

fn rebuild_modal(
    ov: &Overlays,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
) {
    clear(&ov.modal);
    let mode = fe.borrow().core.mode.clone();
    match mode {
        Mode::Connecting { target_display } => {
            ov.modal.set_size_request(-1, -1);
            connecting(&ov.modal, &target_display, fe, drawing);
            ov.modal.set_visible(true);
        }
        Mode::Disconnected { reason } => {
            ov.modal.set_size_request(-1, -1);
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
        Mode::Command(_) => {
            ov.modal.set_size_request(560, -1);
            command_palette(&ov.modal, fe);
            ov.modal.set_visible(true);
        }
        Mode::Help => {
            help(&ov.modal);
            ov.modal.set_visible(true);
        }
        Mode::ConfirmCloseSession { word_id } => {
            ov.modal.set_size_request(-1, -1);
            confirm_close(&ov.modal, &word_id, fe, drawing);
            ov.modal.set_visible(true);
        }
        Mode::RenameSession { buffer, .. } => {
            ov.modal.set_size_request(-1, -1);
            rename(&ov.modal, &buffer);
            ov.modal.set_visible(true);
        }
        _ => {
            ov.modal.set_size_request(-1, -1);
            ov.modal.set_visible(false);
        }
    }
}

/// The `/`-command palette: the prompt + buffer with a caret at `state.cursor`,
/// and the hint dropdown from `cmd::hint::build_hints` with `state.selected`
/// highlighted. Keyboard-driven (no buttons), mirroring the TUI palette.
fn command_palette(modal: &GtkBox, fe: &Rc<RefCell<Frontend>>) {
    let f = fe.borrow();
    let core = &f.core;
    let Mode::Command(state) = &core.mode else {
        return;
    };

    let row = GtkBox::new(Orientation::Horizontal, 0);
    row.set_halign(Align::Start);
    row.append(&label("/", "kmux-overlay-caret"));
    // Split the buffer at the caret (cursor is kept on a char boundary by
    // dispatch; clamp defensively so a stray value can't panic the slice).
    let cur = {
        let c = state.cursor.min(state.buffer.len());
        if state.buffer.is_char_boundary(c) {
            c
        } else {
            state.buffer.len()
        }
    };
    let before = Label::new(Some(&state.buffer[..cur]));
    before.set_halign(Align::Start);
    row.append(&before);
    row.append(&label("\u{258f}", "kmux-overlay-caret"));
    let after = Label::new(Some(&state.buffer[cur..]));
    after.set_halign(Align::Start);
    row.append(&after);
    modal.append(&row);

    for (i, h) in cmd::hint::build_hints(core).iter().enumerate().take(10) {
        let r = GtkBox::new(Orientation::Horizontal, 12);
        r.add_css_class("kmux-overlay-row");
        if i == state.selected {
            r.add_css_class("selected");
        }
        let disp = Label::new(Some(h.display.as_str()));
        disp.set_halign(Align::Start);
        disp.set_hexpand(true);
        disp.set_xalign(0.0);
        r.append(&disp);
        r.append(&label(h.summary, "kmux-overlay-dim"));
        modal.append(&r);
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

/// The full help overlay: every keybinding from `mode::help_entries`, scrollable
/// (any key closes it via the resolve path). Static content, no `fe` needed.
fn help(modal: &GtkBox) {
    modal.set_size_request(620, 540);
    modal.append(&label(" Help ", "kmux-overlay-title"));
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
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&body));
    modal.append(&scroll);
    modal.append(&label("Press any key to close", "kmux-overlay-dim"));
}

/// Confirm-close dialog. Keyboard `y` confirms; the buttons mirror it.
fn confirm_close(modal: &GtkBox, word_id: &str, fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    let name = fe.borrow().core.mgr.display_name_for(word_id);
    modal.append(&label(" Close session ", "kmux-overlay-title"));
    modal.append(&label(
        &format!("Close \u{201c}{name}\u{201d}? This kills its panes."),
        "kmux-overlay-error",
    ));
    modal.append(&label(
        "y to confirm · any other key cancels",
        "kmux-overlay-dim",
    ));

    let row = GtkBox::new(Orientation::Horizontal, 8);
    let yes = action_button("Close");
    dispatch_button(&yes, Action::ConfirmCloseYes, fe, drawing);
    let no = action_button("Cancel");
    dispatch_button(&no, Action::ExitToNormal, fe, drawing);
    row.append(&yes);
    row.append(&no);
    modal.append(&row);
}

/// Rename input: shows the live buffer + caret. Keyboard-driven (RenameChar /
/// RenameBackspace / RenameSubmit), so no buttons.
fn rename(modal: &GtkBox, buffer: &str) {
    modal.append(&label(" Rename session ", "kmux-overlay-title"));
    modal.append(&input_line("Name: ", buffer));
    modal.append(&label("Enter to save · Esc to cancel", "kmux-overlay-dim"));
}

/// Wire a button to dispatch a (non-effect) action and request a redraw.
fn dispatch_button(
    btn: &Button,
    action: Action,
    fe: &Rc<RefCell<Frontend>>,
    drawing: &DrawingArea,
) {
    let fe = fe.clone();
    let drawing = drawing.clone();
    btn.connect_clicked(move |_| {
        {
            let mut f = fe.borrow_mut();
            let _ = futures::executor::block_on(f.core.dispatch_action(action.clone()));
            f.core.needs_render = true;
        }
        drawing.queue_draw();
    });
}

/// Top-right HUD: live render metrics (mirrors the TUI `render_hud`).
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
            "Disc:{} Gap:{} Lag:{} Sync:{}",
            c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
        ),
    ];
    for line in lines {
        hud.append(&label(&line, "kmux-hud-line"));
    }
    hud.set_visible(true);
}

/// Centered metrics card: connection identity, sink, per-transport traffic
/// totals, and the render summary. A subset of the TUI metrics overlay (the
/// per-category and RTT detail are a follow-up).
fn update_metrics(card: &GtkBox, core: &AppCore) {
    if !core.metrics_overlay_visible {
        card.set_visible(false);
        return;
    }
    card.set_size_request(640, -1);
    clear(card);
    let mgr = &core.mgr;
    let metrics = &mgr.metrics;
    card.append(&label(" Metrics ", "kmux-overlay-title"));

    let conn = mgr
        .connection_id
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "-".into());
    card.append(&label(
        &format!("pid {}   connection {conn}", std::process::id()),
        "kmux-overlay-dim",
    ));
    let sink = match metrics.sink_path() {
        Some(p) => format!("sink: {}", p.display()),
        None => "sink: (disabled)".to_string(),
    };
    card.append(&label(&sink, "kmux-overlay-dim"));

    let by_transport = metrics.network.snapshot_by_transport();
    if by_transport.is_empty() {
        card.append(&label("(no transport traffic yet)", "kmux-overlay-dim"));
    } else {
        for (key, totals) in &by_transport {
            card.append(&label(
                &format!("{} {}", key.kind, key.address),
                "kmux-overlay-title",
            ));
            card.append(&label(
                &format!(
                    "   in {}  out {}   msgs {}/{}",
                    fmt_bytes(totals.bytes_in),
                    fmt_bytes(totals.bytes_out),
                    totals.msgs_in,
                    totals.msgs_out,
                ),
                "kmux-hud-line",
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
        "kmux-hud-line",
    ));
    card.append(&label(
        &format!(
            "Disc:{} Gap:{} Lag:{} Sync:{}",
            c.stale_discards, c.seqno_gaps, c.lag_events, c.resyncs
        ),
        "kmux-hud-line",
    ));
    card.set_visible(true);
}

/// Human-readable byte count (B/KB/MB/GB).
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
