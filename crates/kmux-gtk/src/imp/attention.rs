//! Native desktop notifications for `kmux notify` attentions (issue #169).
//!
//! The daemon broadcasts a `PaneAttention` to every connected client, so every
//! window of this single GTK process receives it and emits a
//! [`FrontendEffect::Attention`](kmux_app::driver::FrontendEffect::Attention).
//! We dedup on the server-assigned `attention_id` so exactly one notification is
//! posted, then route its click through an app-scoped `gio` action that
//! refocuses the best window for the session and selects the pane.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk4::{Application, DrawingArea, gio, glib};
use kmux_app::core::TopBarAction;
use kmux_app::mode::Action;
use kmux_protocol::messages::AttentionKind;

use super::Frontend;

/// App-scoped action name; `FULL_ACTION` is what the notification references.
const ACTION: &str = "kmux-attention-focus";
const FULL_ACTION: &str = "app.kmux-attention-focus";
/// Cap on remembered attention ids (dedup ring).
const MAX_SEEN: usize = 256;

thread_local! {
    /// Live windows, for routing a notification click to the right one. Pruned
    /// when a window closes (see [`register_window`]). GTK is single-threaded on
    /// the main loop, so a `thread_local` + `RefCell` is the idiomatic store.
    static WINDOWS: RefCell<Vec<WindowEntry>> = const { RefCell::new(Vec::new()) };
    /// Attention ids already surfaced, so the N windows attached to one session
    /// post exactly one notification. Bounded FIFO.
    static SEEN: RefCell<VecDeque<u64>> = const { RefCell::new(VecDeque::new()) };
}

struct WindowEntry {
    window: adw::ApplicationWindow,
    fe: Weak<RefCell<Frontend>>,
    drawing: DrawingArea,
}

/// Register a window so attention clicks can target it. Call once per window.
pub(super) fn register_window(
    window: &adw::ApplicationWindow,
    fe: &Rc<RefCell<Frontend>>,
    drawing: &DrawingArea,
) {
    WINDOWS.with(|w| {
        w.borrow_mut().push(WindowEntry {
            window: window.clone(),
            fe: Rc::downgrade(fe),
            drawing: drawing.clone(),
        });
    });
    // Drop the entry when the window closes so a stale window is never a target.
    let win = window.clone();
    window.connect_close_request(move |_| {
        WINDOWS.with(|w| w.borrow_mut().retain(|e| e.window != win));
        glib::Propagation::Proceed
    });
}

/// Surface a `kmux notify` attention as a native desktop notification, deduped
/// by `attention_id` so it fires once across the process's windows.
pub(super) fn surface(
    app: &Application,
    word_id: String,
    pane_id: String,
    kind: AttentionKind,
    title: String,
    body: String,
    attention_id: u64,
) {
    let fresh = SEEN.with(|s| {
        let mut seen = s.borrow_mut();
        if seen.contains(&attention_id) {
            return false;
        }
        seen.push_back(attention_id);
        if seen.len() > MAX_SEEN {
            seen.pop_front();
        }
        true
    });
    if !fresh {
        return;
    }

    ensure_action(app);

    let notif = gio::Notification::new(&title);
    notif.set_body(Some(&body));
    // A blocked agent (NeedsInput) is more urgent than a completed turn.
    notif.set_priority(match kind {
        AttentionKind::NeedsInput => gio::NotificationPriority::Urgent,
        AttentionKind::TurnDone => gio::NotificationPriority::Normal,
    });
    let target = (word_id, pane_id).to_variant();
    notif.set_default_action_and_target_value(FULL_ACTION, Some(&target));
    app.send_notification(Some(&format!("kmux-attention-{attention_id}")), &notif);
}

/// Register the app-scoped focus action once. A notification click routes here
/// with the `(word_id, pane_id)` the notification was posted for.
fn ensure_action(app: &Application) {
    if app.lookup_action(ACTION).is_some() {
        return;
    }
    let ty = glib::VariantTy::new("(ss)").expect("valid variant type");
    let act = gio::SimpleAction::new(ACTION, Some(ty));
    let app = app.clone();
    act.connect_activate(move |_, param| {
        if let Some((word_id, pane_id)) = param.and_then(glib::Variant::get::<(String, String)>) {
            focus(&word_id, &pane_id);
        }
    });
    app.add_action(&act);
}

/// Bring the best window for `word_id` to the front and select `pane_id`.
fn focus(word_id: &str, pane_id: &str) {
    WINDOWS.with(|w| {
        let windows = w.borrow();
        let Some(entry) = select_target(&windows, word_id) else {
            return;
        };
        let Some(fe) = entry.fe.upgrade() else {
            return;
        };
        entry.window.present();
        {
            let mut f = fe.borrow_mut();
            if let Some(idx) = f
                .core
                .mgr
                .session_list()
                .iter()
                .position(|e| e.meta.word_id == word_id)
            {
                futures::executor::block_on(f.core.dispatch_action(Action::JumpToSession(idx)));
            }
            // Select the specific pane within the (now active) session.
            let _ = f
                .core
                .apply_top_bar_action(TopBarAction::SelectPane(pane_id.to_string()));
        }
        entry.drawing.grab_focus();
        entry.drawing.queue_draw();
    });
}

/// Choose which window a notification click focuses: prefer a *visible* window
/// already showing the session (the active one if several do), else the active
/// window, else any visible window — it switches to the session on click.
fn select_target<'a>(windows: &'a [WindowEntry], word_id: &str) -> Option<&'a WindowEntry> {
    let visible: Vec<&WindowEntry> = windows.iter().filter(|e| e.window.is_visible()).collect();
    let shows = |e: &WindowEntry| {
        e.fe.upgrade()
            .map(|fe| fe.borrow().core.mgr.active_session() == Some(word_id))
            .unwrap_or(false)
    };
    visible
        .iter()
        .copied()
        .find(|e| e.window.is_active() && shows(e))
        .or_else(|| visible.iter().copied().find(|e| shows(e)))
        .or_else(|| visible.iter().copied().find(|e| e.window.is_active()))
        .or_else(|| visible.first().copied())
}
