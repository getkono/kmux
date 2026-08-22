//! The header bar as a view of `AppCore`: window title (server + active
//! session), a connection-status indicator that reconnects on click, a
//! server-switch button, and a lock indicator.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::Application;

use kmux_app::core::TopBarAction;
use kmux_client::connection_state::ConnectionState;

use super::shell::Shell;
use super::{Frontend, apply_effects};

/// CSS classes the connection indicator toggles between (libadwaita semantic
/// colors), cleared before applying the current one.
const CONN_CLASSES: [&str; 4] = ["success", "warning", "error", "dim-label"];

/// Wire the header buttons. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, app: &Application) {
    // Server button → open the unified launcher (issue #121). Remotes are now
    // federated into the local hub and managed from the launcher, so this is the
    // single entry point for "connect to / open a session on another machine".
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.server_btn.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                f.core.apply_top_bar_action(TopBarAction::OpenLaunchPicker);
                f.core.request_render();
            }
            s.drawing.queue_draw();
        });
    }

    // Connection indicator → reconnect.
    {
        let s = shell.clone();
        let fe = fe.clone();
        let app = app.clone();
        shell.conn_btn.connect_clicked(move |_| {
            let effects = {
                let mut f = fe.borrow_mut();
                let e = f.core.apply_top_bar_action(TopBarAction::Reconnect);
                f.core.request_render();
                e
            };
            apply_effects(&fe, effects, &app, &s.drawing);
            s.drawing.queue_draw();
        });
    }

    // Transport indicator → double-click opens the override chooser (issue #69).
    // The command palette (pre-filled `transport `) is the selection mechanism.
    {
        let s = shell.clone();
        let fe = fe.clone();
        let gesture = gtk4::GestureClick::new();
        gesture.connect_pressed(move |_g, n_press, _, _| {
            if n_press == 2 {
                {
                    let mut f = fe.borrow_mut();
                    f.core.open_transport_chooser();
                    f.core.request_render();
                }
                s.drawing.queue_draw();
            }
        });
        shell.transport_btn.add_controller(gesture);
    }
}

/// Refresh the header from current state. Cheap no-op when unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    use kmux_app::core::PauseReason;
    let (sig, server, subtitle, icon, tip, class, locked, transport, connected, overridden) = {
        let f = fe.borrow();
        let mgr = &f.core.mgr;
        let server = f.core.server_display.clone();
        let session = mgr
            .active_session()
            .map(|w| mgr.display_name_for(w))
            .unwrap_or_default();
        let state = mgr.connection_state();
        let (icon, tip, class) = conn_visual(state);
        let locked = mgr.active_input_locked();
        let connected = state.is_live();
        let overridden = mgr.transport_override().is_some();
        let transport = if connected {
            mgr.current_transport.to_string()
        } else {
            String::new()
        };
        // Pause indicator (issue #68): surfaced in the subtitle so the user can
        // tell the stream is intentionally stopped (vs. a stall).
        let pause = match f.core.pause_reason() {
            PauseReason::None => "",
            PauseReason::Manual => " · ⏸ Paused",
            PauseReason::Auto => " · ⏸ Paused (background)",
        };
        let subtitle = format!("{session}{pause}");
        let sig = format!(
            "{server}|{subtitle}|{}|{locked}|{transport}|{overridden}",
            state.badge_label()
        );
        (
            sig, server, subtitle, icon, tip, class, locked, transport, connected, overridden,
        )
    };
    if shell.header_sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *shell.header_sig.borrow_mut() = Some(sig);

    shell
        .title
        .set_title(if server.is_empty() { "kmux" } else { &server });
    shell.title.set_subtitle(&subtitle);

    shell.conn_btn.set_icon_name(icon);
    shell.conn_btn.set_tooltip_text(Some(tip));
    for c in CONN_CLASSES {
        shell.conn_btn.remove_css_class(c);
    }
    shell.conn_btn.add_css_class(class);

    // Transport indicator: visible only when connected; "warning" (amber) style
    // signals an active override, "dim-label" the auto default (issue #69).
    shell.transport_btn.set_visible(connected);
    shell.transport_btn.set_label(&transport);
    shell.transport_btn.remove_css_class("warning");
    shell.transport_btn.remove_css_class("dim-label");
    if overridden {
        shell.transport_btn.add_css_class("warning");
        shell
            .transport_btn
            .set_tooltip_text(Some("Transport overridden — double-click to change"));
    } else {
        shell.transport_btn.add_css_class("dim-label");
        shell
            .transport_btn
            .set_tooltip_text(Some("Transport protocol — double-click to override"));
    }

    shell.lock_btn.set_visible(locked);
}

/// (icon, tooltip, css class) for a connection state.
fn conn_visual(state: &ConnectionState) -> (&'static str, &'static str, &'static str) {
    match state {
        ConnectionState::Connected { .. } => {
            ("network-transmit-receive-symbolic", "Connected", "success")
        }
        ConnectionState::Handshaking | ConnectionState::Reconnecting { .. } => {
            ("network-transmit-symbolic", "Connecting…", "warning")
        }
        ConnectionState::Disconnected { .. } => (
            "network-offline-symbolic",
            "Disconnected — click to reconnect",
            "error",
        ),
        ConnectionState::Idle => ("network-idle-symbolic", "Idle", "dim-label"),
    }
}
