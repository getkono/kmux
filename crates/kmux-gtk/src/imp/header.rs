//! The header bar as a view of `AppCore`: window title (server + active
//! session), a connection-status indicator that reconnects on click, a
//! server-switch button, and a lock indicator. Replaces the TUI session/status
//! bars (`chrome.rs`).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4::Application;

use kmux_app::core::TopBarAction;
use kmux_client::connection_state::ConnectionState;

use crate::shell::Shell;
use crate::{Frontend, apply_effects};

/// CSS classes the connection indicator toggles between (libadwaita semantic
/// colors), cleared before applying the current one.
const CONN_CLASSES: [&str; 4] = ["success", "warning", "error", "dim-label"];

/// Wire the header buttons. Called once.
pub fn wire(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>, app: &Application) {
    // Server button → open the server picker.
    {
        let s = shell.clone();
        let fe = fe.clone();
        shell.server_btn.connect_clicked(move |_| {
            {
                let mut f = fe.borrow_mut();
                f.core.apply_top_bar_action(TopBarAction::OpenServerPicker);
                f.core.needs_render = true;
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
                f.core.needs_render = true;
                e
            };
            apply_effects(&fe, effects, &app, &s.drawing);
            s.drawing.queue_draw();
        });
    }
}

/// Refresh the header from current state. Cheap no-op when unchanged.
pub fn sync(shell: &Rc<Shell>, fe: &Rc<RefCell<Frontend>>) {
    let (sig, server, session, icon, tip, class, locked) = {
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
        let sig = format!("{server}|{session}|{}|{locked}", state.badge_label());
        (sig, server, session, icon, tip, class, locked)
    };
    if shell.header_sig.borrow().as_deref() == Some(sig.as_str()) {
        return;
    }
    *shell.header_sig.borrow_mut() = Some(sig);

    shell
        .title
        .set_title(if server.is_empty() { "kmux" } else { &server });
    shell.title.set_subtitle(&session);

    shell.conn_btn.set_icon_name(icon);
    shell.conn_btn.set_tooltip_text(Some(tip));
    for c in CONN_CLASSES {
        shell.conn_btn.remove_css_class(c);
    }
    shell.conn_btn.add_css_class(class);

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
