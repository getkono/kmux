//! Native GTK chrome: the session (top) bar, status bar, and hint bar.
//!
//! The bars are *views* of `AppCore` state, rebuilt by [`sync`] when the
//! relevant state changes (gated by a cheap signature so terminal output does
//! not churn widgets). Interactive segments are real `GtkButton`s whose click
//! handlers call [`AppCore::apply_top_bar_action`] — the same policy the TUI's
//! mouse handler uses — so behavior has one source of truth. Mirrors
//! `kmux-tui/src/ui/bars.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Application, Box as GtkBox, Button, DrawingArea, Label, Orientation};

use kmux_app::core::AppCore;
use kmux_app::mode;
use kmux_client::connection_state::ConnectionState;
use kmux_protocol::dirs::BuildProfile;

use crate::{Frontend, handle_effect};

/// The three chrome bars plus the last-rendered signature (to skip rebuilds).
pub struct Bars {
    pub session: GtkBox,
    pub status: GtkBox,
    pub hint: GtkBox,
    last_sig: RefCell<Option<String>>,
}

/// Build the (empty) bars. [`sync`] populates them from state.
pub fn build() -> Bars {
    let session = GtkBox::new(Orientation::Horizontal, 0);
    session.add_css_class("kmux-session-bar");
    let status = GtkBox::new(Orientation::Horizontal, 0);
    status.add_css_class("kmux-status-bar");
    let hint = GtkBox::new(Orientation::Horizontal, 0);
    hint.add_css_class("kmux-hint-bar");
    Bars {
        session,
        status,
        hint,
        last_sig: RefCell::new(None),
    }
}

/// Remove all children of a box.
fn clear(b: &GtkBox) {
    while let Some(child) = b.first_child() {
        b.remove(&child);
    }
}

/// A compact fingerprint of everything the bars display. When unchanged we skip
/// rebuilding so high-rate terminal output doesn't churn widgets every frame.
fn signature(core: &AppCore) -> String {
    let mgr = &core.mgr;
    let panes: String = mgr
        .active_session_panes()
        .iter()
        .map(|p| format!("{}:{}:{};", p.pane_index, p.pane_id, p.title))
        .collect();
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        core.server_display,
        mgr.connection_state().badge_label(),
        mgr.active_session().unwrap_or(""),
        mgr.active_pane_id().unwrap_or(""),
        panes,
        mgr.session_list().len(),
        mgr.active_input_locked(),
        mgr.host_port_display(),
        mgr.status_msg(),
        mode::mode_name(&core.mode),
    )
}

/// Rebuild the bars from current state if it changed. Wires fresh click handlers
/// (each capturing clones of `fe`/`app`/`drawing`).
pub fn sync(bars: &Bars, fe: &Rc<RefCell<Frontend>>, app: &Application, drawing: &DrawingArea) {
    let sig = signature(&fe.borrow().core);
    if bars.last_sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *bars.last_sig.borrow_mut() = Some(sig);

    let f = fe.borrow();
    let core = &f.core;
    let mgr = &core.mgr;

    // ── Session (top) bar ──
    clear(&bars.session);
    bars.session.append(&badge(
        &format!(" {} ", core.server_display),
        "kmux-server",
        fe,
        app,
        drawing,
        Some(kmux_app::core::TopBarAction::OpenServerPicker),
    ));
    let state = mgr.connection_state();
    let conn = badge(
        &format!(" {} ", state.badge_label()),
        connection_class(state),
        fe,
        app,
        drawing,
        Some(kmux_app::core::TopBarAction::Reconnect),
    );
    bars.session.append(&conn);

    let session_text = match mgr.active_session() {
        Some(word_id) => format!(" \u{25b6} {} ", mgr.display_name_for(word_id)),
        None => " No sessions ".to_string(),
    };
    bars.session.append(&badge(
        &session_text,
        "kmux-session",
        fe,
        app,
        drawing,
        Some(kmux_app::core::TopBarAction::OpenSessionPicker),
    ));

    let active_pane = mgr.active_pane_id().map(|s| s.to_string());
    let panes = mgr.active_session_panes();
    if panes.is_empty() {
        let dash = Label::new(Some(" \u{2014} "));
        dash.add_css_class("kmux-pane-empty");
        bars.session.append(&dash);
    } else {
        for pane in panes {
            let is_active = active_pane.as_deref() == Some(pane.pane_id.as_str());
            let dot = if is_active { "\u{2022}" } else { "" };
            let label = format!(" {dot}{} {} ", pane.pane_index, pane.title);
            let btn = badge(
                &label,
                "kmux-pane",
                fe,
                app,
                drawing,
                Some(kmux_app::core::TopBarAction::SelectPane(
                    pane.pane_id.clone(),
                )),
            );
            if is_active {
                btn.add_css_class("active");
            }
            // Keep long titles from pushing later tabs off-screen.
            if let Some(lbl) = btn.child().and_downcast::<Label>() {
                lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                lbl.set_max_width_chars(22);
            }
            bars.session.append(&btn);
        }
    }
    if mgr.active_session().is_some() {
        bars.session.append(&badge(
            " + ",
            "kmux-add-pane",
            fe,
            app,
            drawing,
            Some(kmux_app::core::TopBarAction::CreatePane),
        ));
    }

    // ── Status bar ──
    clear(&bars.status);
    let host_port = mgr.host_port_display();
    if !host_port.is_empty() {
        bars.status
            .append(&status_label(&format!(" {host_port} "), "kmux-hostport"));
    }
    bars.status.append(&status_label(
        &format!("{} sessions", mgr.session_list().len()),
        "kmux-sessions",
    ));
    if mgr.active_input_locked() {
        bars.status.append(&status_label(" LOCKED ", "kmux-locked"));
    }
    if let Some((rows, cols)) = mgr.active_term_size() {
        bars.status
            .append(&status_label(&format!(" {cols}x{rows}"), "kmux-dims"));
    }
    // Spacer pushes the right-hand items to the edge.
    let spacer = GtkBox::new(Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bars.status.append(&spacer);
    let status_msg = mgr.status_msg();
    if !status_msg.is_empty() {
        bars.status
            .append(&status_label(&format!(" {status_msg} "), "kmux-status-msg"));
    }
    if BuildProfile::CURRENT == BuildProfile::Debug {
        bars.status
            .append(&status_label(" DEBUG ", "kmux-debug-badge"));
    }

    // ── Hint bar ──
    clear(&bars.hint);
    let mode_badge = status_label(
        &format!(" {} ", mode::mode_name(&core.mode)),
        "kmux-mode-badge",
    );
    bars.hint.append(&mode_badge);
    for (key, desc) in mode::mode_hints(&core.mode) {
        let chip = status_label(&format!(" {key} "), "kmux-hint-key");
        bars.hint.append(&chip);
        bars.hint
            .append(&status_label(&format!(" {desc} "), "kmux-hint-desc"));
    }
}

/// A clickable top-bar segment. The click applies `action` via `AppCore` and
/// routes any resulting effect (e.g. Reconnect) through `handle_effect`.
fn badge(
    text: &str,
    css: &str,
    fe: &Rc<RefCell<Frontend>>,
    app: &Application,
    drawing: &DrawingArea,
    action: Option<kmux_app::core::TopBarAction>,
) -> Button {
    let btn = Button::with_label(text);
    btn.add_css_class("kmux-badge");
    btn.add_css_class(css);
    btn.set_can_focus(false);
    if let Some(action) = action {
        let fe = fe.clone();
        let app = app.clone();
        let drawing = drawing.clone();
        btn.connect_clicked(move |_| {
            let result = {
                let mut f = fe.borrow_mut();
                let r = f.core.apply_top_bar_action(action.clone());
                f.core.needs_render = true;
                r
            };
            if let Some(result) = result {
                handle_effect(&fe, result, &app, &drawing);
            }
            drawing.queue_draw();
        });
    }
    btn
}

fn status_label(text: &str, css: &str) -> Label {
    let label = Label::new(Some(text));
    label.add_css_class(css);
    label
}

/// CSS class for the connection badge, keyed by state, so the theme can color it.
fn connection_class(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected { .. } => "kmux-conn-connected",
        ConnectionState::Handshaking | ConnectionState::Reconnecting { .. } => {
            "kmux-conn-connecting"
        }
        ConnectionState::Disconnected { .. } => "kmux-conn-disconnected",
        ConnectionState::Idle => "kmux-conn-idle",
    }
}
