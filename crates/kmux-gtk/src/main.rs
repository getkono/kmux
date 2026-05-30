//! GTK4 frontend for kmux.
//!
//! The toolkit-agnostic [`kmux_app::core::AppCore`] drives this native GTK
//! frontend exactly as it drives the TUI: a glib main-loop *pump* polls the
//! core's network channels and ticks its timers, a `DrawingArea` renders
//! `AppCore`'s active grid, and GDK key events are converted to the shared key
//! model and fed through `mode::resolve` → `AppCore::dispatch_action`.
//!
//! `AppCore` is *driven, not driving* — only the pump and the render/input
//! leaves are GTK-specific. The pump mirrors the arms of the TUI's
//! `tokio::select!` loop (`kmux-tui/src/app/event_loop.rs`).

mod actions;
mod convert;
mod css;
mod dialogs;
mod header;
mod input;
mod prefs;
mod render;
mod shell;
mod sidebar;
mod tabs;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk4::{Application, DrawingArea, EventControllerKey, gdk, gio, glib};

use kmux_app::core::{AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult, SwitchTarget};
use kmux_app::launch::{Launch, Plan, run_cli};
use kmux_app::mode::Mode;
use kmux_app::theme::Theme;
use kmux_client::connection_state::DisconnectReason;
use kmux_client::generate_instance_id;
use kmux_client::supervisor::UpgradeSignal;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{ClientCapabilities, ClientMessage, ServerMessage, TermSize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

const APP_ID: &str = "dev.getkono.kmux";

/// Pump cadence: drain network channels + tick timers (~60 Hz).
const PUMP_INTERVAL: Duration = Duration::from_millis(16);
/// Liveness ping + timeout evaluation cadence (matches the TUI).
const LIVENESS_TICK: Duration = Duration::from_secs(1);
/// Metrics JSONL flush cadence (matches the TUI / `docs/metrics.md`).
const METRICS_FLUSH_TICK: Duration = Duration::from_secs(10);
/// Debounce window for window-resize bursts; GTK fires many size-allocations
/// during a drag, so coalesce them into one `set_term_size`.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);
/// Cursor blink half-period (on→off or off→on). Matches GTK's default
/// `gtk-cursor-blink-time` (1200 ms full cycle) / 2.
const CURSOR_BLINK_HALF: Duration = Duration::from_millis(600);

/// Shared frontend state pumped by the glib loop. The receivers are the same
/// channels the TUI's event loop owns; here the glib pump drains them instead
/// of a `tokio::select!`.
struct Frontend {
    core: AppCore,
    /// Server messages for the live connection. Replaced on reconnect / server
    /// switch (the old receiver is dropped, closing the stale channel).
    srv_rx: mpsc::UnboundedReceiver<ServerMessage>,
    /// Outcome channel for the in-flight bootstrap; `None` while idle.
    bootstrap_rx: Option<mpsc::UnboundedReceiver<BootstrapTaskResult>>,
    /// Better-transport signals from the background supervisor probe.
    upgrade_rx: mpsc::Receiver<UpgradeSignal>,
    upgrade_tx: mpsc::Sender<UpgradeSignal>,
    /// SSH tunnel-death signal (the tunnel process exited unexpectedly).
    tunnel_died_rx: mpsc::Receiver<()>,
    tunnel_died_tx: mpsc::Sender<()>,
    /// Cell geometry derived from the configured font; recomputed on scale
    /// change. Drives the grid render and the resize → cols/rows mapping.
    metrics: render::Metrics,
    /// The CSS provider for the chrome/overlay theme, reloaded when the palette
    /// changes (`/theme`). The last palette applied to it, for change detection.
    css_provider: gtk4::CssProvider,
    last_palette: Theme,
    /// Timer bookkeeping: the glib pump fires on one interval, so we track each
    /// cadence's last-fire / deadline ourselves.
    last_liveness: Instant,
    last_metrics_flush: Instant,
    pending_resize: Option<TermSize>,
    resize_deadline: Option<Instant>,
    /// Cursor-blink phase: `true` shows the cursor, `false` hides it on the
    /// "off" half of a blink cycle. Only a cursor that requested blinking
    /// (DECSCUSR `blinking_*`) is toggled; a steady cursor stays solid.
    blink_on: bool,
    /// When the current blink half-cycle started; advanced every
    /// [`CURSOR_BLINK_HALF`]. Reset on keypress so typing shows a solid cursor.
    blink_phase_start: Instant,
}

fn main() -> anyhow::Result<()> {
    // A tokio runtime backs AppCore's async orchestration (start_bootstrap spawns
    // tasks) and the CLI front door's daemon/subcommand network calls.
    let rt = tokio::runtime::Runtime::new()?;
    let instance_id = generate_instance_id();
    match rt.block_on(run_cli(instance_id))? {
        Launch::Done => Ok(()),
        Launch::Interactive(plan) => {
            // Enter the runtime on the main thread so the tokio spawns from glib
            // callbacks land on it; the glib loop itself runs on this thread.
            let _guard = rt.enter();
            run_gui(*plan)
        }
    }
}

/// Run the GTK application for an interactive session built from `plan`.
fn run_gui(plan: Plan) -> anyhow::Result<()> {
    let app = Application::builder().application_id(APP_ID).build();
    let plan = Rc::new(plan);
    // A fatal bootstrap error is shown in-window (disconnect overlay) and also
    // surfaced to stderr after teardown, mirroring the TUI's stashed-error path.
    let exit_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Initialize libadwaita once gtk is up (we keep a gtk4::Application rather
    // than adw::Application to avoid threading adw types through every helper).
    app.connect_startup(|_| {
        adw::init().expect("failed to initialize libadwaita");
    });
    {
        let exit_error = exit_error.clone();
        app.connect_activate(move |app| build_ui(app, &plan, exit_error.clone()));
    }
    app.run();
    if let Some(err) = exit_error.borrow_mut().take() {
        eprintln!("kmux: connection failed:\n{err}");
    }
    Ok(())
}

fn build_ui(app: &Application, plan: &Plan, exit_error: Rc<RefCell<Option<String>>>) {
    // GUI capabilities differ from a terminal's: truecolor on, no kitty
    // keyboard/graphics concept.
    let capabilities = ClientCapabilities {
        truecolor: true,
        kitty_graphics: false,
        kitty_keyboard: false,
        term: None,
        term_program: Some("kmux-gtk".to_string()),
    };
    let term_size = TermSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut core = AppCore::new(
        plan.target.clone(),
        plan.initial_cwd.clone(),
        plan.instance_id.clone(),
        plan.auto_session.clone(),
        plan.auto_cwd.clone(),
        capabilities,
        plan.theme.clone(),
        term_size,
    );

    // The frontend owns the network channels (as the TUI's run loop does) and
    // kicks off the initial bootstrap. The upgrade / tunnel-death channels live
    // for the whole session; their senders are cloned into the SSH supervisor.
    let (srv_tx, srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (bs_tx, bootstrap_rx) = mpsc::unbounded_channel::<BootstrapTaskResult>();
    let (upgrade_tx, upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
    let (tunnel_died_tx, tunnel_died_rx) = mpsc::channel::<()>(1);
    let mut bootstrap_rx = Some(bootstrap_rx);
    if let Some(target) = core.pending_target.take() {
        core.start_bootstrap(target, srv_tx, BootstrapPhase::Initial, bs_tx);
    } else {
        bootstrap_rx = None;
    }

    let drawing = DrawingArea::new();
    drawing.set_hexpand(true);
    drawing.set_vexpand(true);
    // Focusable so it can hold keyboard focus; clicking it (and selecting a
    // session/pane) returns focus here so typing goes to the terminal rather
    // than the sidebar list.
    drawing.set_focusable(true);

    // Derive cell geometry from the configured font (the widget's PangoContext
    // carries the display font map + scale). Recomputed on scale-factor change.
    let font = render::font_from_str(&plan.font);
    let metrics = render::Metrics::measure(&drawing.pango_context(), font);

    // Theme the chrome + overlays from the active palette, and match the
    // libadwaita window styling (light/dark) to the theme. Both are refreshed
    // by the pump when `/theme` changes the palette.
    let css_provider = gdk::Display::default()
        .map(|d| css::install(&d, &plan.theme))
        .unwrap_or_default();
    adw::StyleManager::default().set_color_scheme(scheme_for(&plan.theme));

    let now = Instant::now();
    let fe = Rc::new(RefCell::new(Frontend {
        core,
        srv_rx,
        bootstrap_rx,
        upgrade_rx,
        upgrade_tx,
        tunnel_died_rx,
        tunnel_died_tx,
        metrics,
        css_provider,
        last_palette: plan.theme.clone(),
        last_liveness: now,
        last_metrics_flush: now,
        pending_resize: None,
        resize_deadline: None,
        blink_on: true,
        blink_phase_start: now,
    }));

    {
        let fe = fe.clone();
        drawing.set_draw_func(move |area, cr, w, h| {
            let fe = fe.borrow();
            render::render(
                &fe.core,
                cr,
                &area.pango_context(),
                &fe.metrics,
                w,
                h,
                fe.blink_on,
            );
        });
    }

    // Window-resize → debounced term-size update. Event-driven (like the TUI's
    // SIGWINCH) rather than polled; the pump applies it once the burst settles.
    {
        let fe = fe.clone();
        drawing.connect_resize(move |_area, w, h| {
            let mut fe = fe.borrow_mut();
            let (cols, rows) = fe.metrics.cols_rows(w, h);
            fe.pending_resize = Some(TermSize {
                rows,
                cols,
                pixel_width: w.max(0) as u16,
                pixel_height: h.max(0) as u16,
            });
            fe.resize_deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
        });
    }

    // Re-measure cells when the display scale factor changes (e.g. dragging the
    // window between a 1× and a 2× monitor).
    {
        let fe = fe.clone();
        drawing.connect_scale_factor_notify(move |area| {
            {
                let mut fe = fe.borrow_mut();
                let font = fe.metrics.font.clone();
                fe.metrics = render::Metrics::measure(&area.pango_context(), font);
            }
            area.queue_resize();
            area.queue_draw();
        });
    }

    // Native shell: header bar + sessions sidebar + a pane tab strip hosting the
    // shared grid. The modal overlays + HUD ride the shell's inner overlay until
    // they become native dialogs.
    let shell = shell::build(app, &drawing);
    let dialogs = Rc::new(dialogs::build(&shell.overlay));
    header::wire(&shell, &fe, app);
    tabs::wire(&shell, &fe, app);
    sidebar::wire(&shell, &fe, app);

    // The disconnect banner's only button reconnects.
    {
        let fe = fe.clone();
        let shell2 = shell.clone();
        let app = app.clone();
        shell.banner.connect_button_clicked(move |_| {
            handle_effect(&fe, KeyResult::Reconnect, &app, &shell2.drawing);
        });
    }

    actions::install(&shell, &fe, app);

    // Key input: the controller lives on the focused terminal `DrawingArea`, so
    // window/app accelerators (capture phase, at the window) are always evaluated
    // first; only keys the accelerators don't claim reach here and are forwarded
    // to the PTY (the daemon's Ghostty encoder emits the right bytes under the
    // live terminal mode state). There is no modal-chord path in the GUI;
    // commands are accelerators (see actions.rs).
    let key_ctl = EventControllerKey::new();
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        key_ctl.connect_key_pressed(move |_ctl, keyval, _code, gdk_mods| {
            let mut f = fe.borrow_mut();
            // While a dialog / connection overlay owns the UI, leave input to it.
            if !matches!(f.core.mode, Mode::Normal) {
                return glib::Propagation::Proceed;
            }
            if let Some(grid) = f.core.mgr.active_grid_mut() {
                grid.scroll_to_bottom();
            }
            if let Some(proto) = convert::convert_to_protocol_key(keyval, gdk_mods) {
                f.core.mgr.send_key_batch(vec![proto]);
                // Typing shows a solid cursor: restart the blink cycle.
                f.blink_on = true;
                f.blink_phase_start = Instant::now();
            }
            drop(f);
            drawing.queue_draw();
            glib::Propagation::Stop
        });
    }
    drawing.add_controller(key_ctl);

    // Mouse: scroll-wheel (PTY mouse-report or local scrollback).
    input::attach(&drawing, &fe);

    // Populate the shell + overlays once so they aren't blank until the first tick.
    header::sync(&shell, &fe);
    tabs::sync(&shell, &fe);
    sidebar::sync(&shell, &fe);
    dialogs::sync(&dialogs, &shell, &fe, app);

    // The pump: drain network channels, tick timers, sync the shell/dialogs, redraw.
    {
        let fe = fe.clone();
        let shell = shell.clone();
        let dialogs = dialogs.clone();
        let app = app.clone();
        glib::timeout_add_local(PUMP_INTERVAL, move || {
            pump(&fe, &shell, &dialogs, &app);
            glib::ControlFlow::Continue
        });
    }

    // Copy the final fatal error (if any) out for stderr after the loop exits.
    {
        let fe = fe.clone();
        app.connect_shutdown(move |_app| {
            *exit_error.borrow_mut() = fe.borrow().core.last_exit_error.clone();
        });
    }

    shell.window.present();
}

/// One pump tick. Mirrors the arms of the TUI `tokio::select!` loop: settled
/// resize, server messages, bootstrap outcome, transport upgrade, tunnel death,
/// liveness, and metrics flush.
fn pump(
    fe: &Rc<RefCell<Frontend>>,
    shell: &Rc<shell::Shell>,
    dialogs: &Rc<dialogs::Dialogs>,
    app: &Application,
) {
    let redraw = {
        let mut fe = fe.borrow_mut();
        let mut dirty = false;
        let now = Instant::now();

        // ── Reflect a `/theme` palette change onto the chrome CSS + window
        // light/dark styling (the cairo grid reads the palette live). ──
        if !palette_eq(&fe.core.palette, &fe.last_palette) {
            css::reload(&fe.css_provider, &fe.core.palette);
            adw::StyleManager::default().set_color_scheme(scheme_for(&fe.core.palette));
            fe.last_palette = fe.core.palette.clone();
            dirty = true;
        }

        // ── Apply a settled resize (debounced in `connect_resize`). ──
        if let Some(deadline) = fe.resize_deadline
            && now >= deadline
        {
            if let Some(size) = fe.pending_resize.take() {
                fe.core.set_term_size(size);
                dirty = true;
            }
            fe.resize_deadline = None;
        }

        // ── Server messages (batched; skipped once disconnected). ──
        if !matches!(fe.core.mode, Mode::Disconnected { .. }) {
            let mut batch = Vec::new();
            let mut closed = false;
            loop {
                match fe.srv_rx.try_recv() {
                    Ok(m) => batch.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        closed = true;
                        break;
                    }
                }
            }
            if !batch.is_empty() {
                fe.core.mgr.metrics.record_batch(batch.len());
                for m in batch {
                    let events = fe.core.mgr.handle_server_message(m);
                    fe.core.handle_session_events(events);
                }
                dirty = true;
            }
            // Channel closed while live → the connection dropped.
            if closed
                && !matches!(
                    fe.core.mode,
                    Mode::Connecting { .. } | Mode::Disconnected { .. }
                )
            {
                fe.core.enter_disconnected(DisconnectReason::ServerClosed);
                dirty = true;
            }
        }

        // ── Bootstrap outcome (at most one per bootstrap). ──
        let outcome = fe.bootstrap_rx.as_mut().and_then(|rx| match rx.try_recv() {
            Ok(o) => Some(Ok(o)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(())),
        });
        match outcome {
            Some(Ok(BootstrapTaskResult::Success(o))) => {
                fe.core.cancel_tx = None;
                // A later success clears any stashed failure so we don't re-print a
                // stale error when the user finally quits.
                fe.core.last_exit_error = None;
                let ssh_ctx = fe.core.mgr.apply_outcome(*o);
                if let Some(ctx) = ssh_ctx {
                    let srv_tx = fe
                        .core
                        .pending_srv_tx
                        .take()
                        .expect("pending_srv_tx set in start_bootstrap");
                    let upgrade_tx = fe.upgrade_tx.clone();
                    let tunnel_died_tx = fe.tunnel_died_tx.clone();
                    fe.core
                        .launch_ssh_supervisor(ctx, srv_tx, upgrade_tx, tunnel_died_tx);
                } else {
                    fe.core.pending_srv_tx = None;
                }
                fe.core.reflect_bootstrap_outcome();
                fe.bootstrap_rx = None;
                dirty = true;
            }
            Some(Ok(BootstrapTaskResult::Failed(reason))) => {
                fe.core.cancel_tx = None;
                fe.core.pending_srv_tx = None;
                // Stash so it survives teardown and is re-printed to stderr; the
                // disconnect overlay shows the same text in-window.
                fe.core.last_exit_error = Some(reason.clone());
                fe.core
                    .enter_disconnected(DisconnectReason::BootstrapFailed(reason));
                fe.bootstrap_rx = None;
                dirty = true;
            }
            Some(Err(())) => {
                // Channel closed with no result → the bootstrap was cancelled
                // (cancel_tx dropped via Action::CancelBootstrap).
                fe.core.cancel_tx = None;
                fe.core.pending_srv_tx = None;
                if matches!(fe.core.mode, Mode::Connecting { .. }) {
                    fe.core
                        .enter_disconnected(DisconnectReason::BootstrapFailed(
                            "cancelled".to_string(),
                        ));
                }
                fe.bootstrap_rx = None;
                dirty = true;
            }
            None => {}
        }

        // ── Transport upgrade (a better transport was found by the probe). ──
        while let Ok(signal) = fe.upgrade_rx.try_recv() {
            let _ = signal.sender.send(ClientMessage::ChannelReady);
            fe.core
                .mgr
                .apply_transport_upgrade(signal.sender, signal.new_kind);
            dirty = true;
        }

        // ── SSH tunnel death (freeze if we're on the tunnelled transport). ──
        while fe.tunnel_died_rx.try_recv().is_ok() {
            if fe.core.mgr.current_transport == TransportKind::TcpTls
                && !matches!(fe.core.mode, Mode::Disconnected { .. })
            {
                fe.core.enter_disconnected(DisconnectReason::SshTunnelDied);
                dirty = true;
            }
        }

        // ── Liveness ping + timeout (1 s). ──
        if now.duration_since(fe.last_liveness) >= LIVENESS_TICK {
            fe.last_liveness = now;
            fe.core.mgr.maybe_send_client_ping(now);
            if fe.core.mgr.is_liveness_timed_out(now)
                && !matches!(
                    fe.core.mode,
                    Mode::Disconnected { .. } | Mode::Connecting { .. }
                )
            {
                fe.core.enter_disconnected(DisconnectReason::PingTimeout);
                dirty = true;
            }
        }

        // ── Metrics JSONL flush (10 s). ──
        if now.duration_since(fe.last_metrics_flush) >= METRICS_FLUSH_TICK {
            fe.last_metrics_flush = now;
            let conn_id = fe.core.mgr.connection_id;
            fe.core.mgr.metrics.flush_sample(conn_id);
        }

        // Keep the HUD refreshing while it is shown (the metrics dialog is a
        // snapshot taken when it opens, so it doesn't need a per-frame tick).
        if fe.core.hud_visible {
            dirty = true;
        }
        // ── Cursor blink. Drive the cycle off the pump: a cursor that requested
        // blinking (DECSCUSR `blinking_*`) toggles every CURSOR_BLINK_HALF; a
        // steady cursor stays solid. (Shape == Hidden ⇒ !visible, so checking
        // `visible && blink` already excludes hidden cursors.) ──
        let cursor_blinks = fe.core.mgr.active_grid().is_some_and(|g| {
            let c = g.cursor();
            c.visible && c.blink
        });
        let (blink_on, blink_start, blink_changed) =
            advance_blink(fe.blink_on, fe.blink_phase_start, cursor_blinks, now);
        fe.blink_on = blink_on;
        fe.blink_phase_start = blink_start;
        dirty |= blink_changed;

        let redraw = dirty || fe.core.needs_render;
        fe.core.needs_render = false;
        redraw
    };
    // The borrow is released; reconcile the native shell + overlays (cheap when
    // state is unchanged) and repaint the grid.
    if redraw {
        header::sync(shell, fe);
        tabs::sync(shell, fe);
        sidebar::sync(shell, fe);
        dialogs::sync(dialogs, shell, fe, app);
        shell.drawing.queue_draw();
    }
}

/// Advance the cursor-blink phase for one pump tick.
///
/// Given the current phase (`blink_on`), when the current half-cycle started
/// (`phase_start`), whether the active cursor is currently *requesting* blink
/// (`cursor_blinks`), and `now`, returns `(new_blink_on, new_phase_start,
/// changed)`. `changed` drives a redraw.
///
/// - A blinking cursor toggles once a full [`CURSOR_BLINK_HALF`] has elapsed.
/// - A non-blinking (steady) cursor is pinned solid; if it was mid-"off" the
///   pin counts as a change so the solid cursor repaints immediately.
fn advance_blink(
    blink_on: bool,
    phase_start: Instant,
    cursor_blinks: bool,
    now: Instant,
) -> (bool, Instant, bool) {
    if cursor_blinks {
        if now.duration_since(phase_start) >= CURSOR_BLINK_HALF {
            (!blink_on, now, true)
        } else {
            (blink_on, phase_start, false)
        }
    } else if !blink_on {
        (true, phase_start, true)
    } else {
        (blink_on, phase_start, false)
    }
}

/// Perform the toolkit-specific follow-up for a dispatch result: quit, clipboard
/// copy/paste, and the reconnect / server-switch channel rebuilds.
fn handle_effect(
    fe: &Rc<RefCell<Frontend>>,
    result: KeyResult,
    app: &Application,
    drawing: &DrawingArea,
) {
    match result {
        KeyResult::Continue => {}
        KeyResult::Quit => app.quit(),
        KeyResult::CopyToClipboard(text) => {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
        KeyResult::RequestPaste => {
            let Some(display) = gdk::Display::default() else {
                return;
            };
            // Clipboard reads are async in GTK; feed the text back when it lands.
            let fe = fe.clone();
            let drawing = drawing.clone();
            display
                .clipboard()
                .read_text_async(gio::Cancellable::NONE, move |res| {
                    if let Ok(Some(text)) = res {
                        fe.borrow_mut().core.mgr.send_paste(text.to_string());
                        drawing.queue_draw();
                    }
                });
        }
        KeyResult::Reconnect => {
            reconnect(&mut fe.borrow_mut());
            drawing.queue_draw();
        }
        KeyResult::SwitchServer(target) => {
            switch_server(&mut fe.borrow_mut(), target);
            drawing.queue_draw();
        }
    }
}

/// Rebuild the server + bootstrap channels and start a fresh bootstrap to the
/// current target. The SSH supervisor (if any) is launched from the pump when
/// the bootstrap completes, using the senders stored on `Frontend`.
fn reconnect(fe: &mut Frontend) {
    let (srv_tx, srv_rx) = mpsc::unbounded_channel();
    fe.srv_rx = srv_rx;
    let (bs_tx, bs_rx) = mpsc::unbounded_channel();
    fe.bootstrap_rx = Some(bs_rx);
    let target = fe.core.current_target();
    fe.core
        .start_bootstrap(target, srv_tx, BootstrapPhase::Reconnect, bs_tx);
    fe.core.needs_render = true;
}

/// Apply a server-picker selection: AppCore mutates the server identity and
/// returns the target; the frontend rebuilds channels and bootstraps it.
fn switch_server(fe: &mut Frontend, target: SwitchTarget) {
    let (srv_tx, srv_rx) = mpsc::unbounded_channel();
    fe.srv_rx = srv_rx;
    let resolved = fe.core.prepare_switch(&target);
    let (bs_tx, bs_rx) = mpsc::unbounded_channel();
    fe.bootstrap_rx = Some(bs_rx);
    fe.core
        .start_bootstrap(resolved, srv_tx, BootstrapPhase::Initial, bs_tx);
    fe.core.needs_render = true;
}

/// Pick the libadwaita color scheme matching a theme's background luminance, so
/// the window frame / unstyled chrome follows the active kmux theme.
fn scheme_for(t: &Theme) -> adw::ColorScheme {
    let bg = t.bg;
    let lum = 0.299 * bg.r as f64 + 0.587 * bg.g as f64 + 0.114 * bg.b as f64;
    if lum < 128.0 {
        adw::ColorScheme::PreferDark
    } else {
        adw::ColorScheme::PreferLight
    }
}

/// Whether two palettes are identical. `Theme` is not `PartialEq`, but `Rgb` is.
fn palette_eq(a: &Theme, b: &Theme) -> bool {
    a.bg == b.bg
        && a.fg == b.fg
        && a.fg_dim == b.fg_dim
        && a.accent == b.accent
        && a.green == b.green
        && a.red == b.red
        && a.yellow == b.yellow
        && a.purple == b.purple
        && a.orange == b.orange
        && a.status_bg == b.status_bg
}

#[cfg(test)]
mod tests {
    use super::{CURSOR_BLINK_HALF, advance_blink};
    use std::time::Instant;

    #[test]
    fn blinking_cursor_holds_phase_until_half_elapsed() {
        let t0 = Instant::now();
        // Just shy of the half-period: no toggle.
        let (on, start, changed) = advance_blink(true, t0, true, t0 + CURSOR_BLINK_HALF / 2);
        assert!(on, "still on");
        assert_eq!(start, t0, "phase start unchanged");
        assert!(!changed, "no redraw before the half-period");
    }

    #[test]
    fn blinking_cursor_toggles_after_half_period() {
        let t0 = Instant::now();
        let (on, start, changed) = advance_blink(true, t0, true, t0 + CURSOR_BLINK_HALF);
        assert!(!on, "toggled off");
        assert_eq!(start, t0 + CURSOR_BLINK_HALF, "phase restarts at now");
        assert!(changed, "toggle forces a redraw");
        // And back on after another half-period.
        let (on2, _, changed2) = advance_blink(on, start, true, start + CURSOR_BLINK_HALF);
        assert!(on2, "toggled back on");
        assert!(changed2);
    }

    #[test]
    fn steady_cursor_stays_solid_and_never_toggles() {
        let t0 = Instant::now();
        // Already on + not blinking → no change even long after the period.
        let (on, _, changed) = advance_blink(true, t0, false, t0 + CURSOR_BLINK_HALF * 10);
        assert!(on);
        assert!(!changed, "a steady cursor must not blink");
    }

    #[test]
    fn switching_to_steady_mid_off_restores_solid_cursor() {
        let t0 = Instant::now();
        // Cursor was mid-"off" (blink_on=false) and is no longer blinking →
        // restore solid and force one redraw.
        let (on, _, changed) = advance_blink(false, t0, false, t0);
        assert!(on, "restored to solid");
        assert!(changed, "repaint the now-solid cursor");
    }
}
