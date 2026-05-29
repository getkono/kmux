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

mod convert;
mod render;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, DrawingArea, EventControllerKey, gdk, gio, glib};

use kmux_app::core::{AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult, SwitchTarget};
use kmux_app::launch::{Launch, Plan, run_cli};
use kmux_app::mode::{self, Action, Mode};
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
    /// Timer bookkeeping: the glib pump fires on one interval, so we track each
    /// cadence's last-fire / deadline ourselves.
    last_liveness: Instant,
    last_metrics_flush: Instant,
    pending_resize: Option<TermSize>,
    resize_deadline: Option<Instant>,
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

    // Derive cell geometry from the configured font (the widget's PangoContext
    // carries the display font map + scale). Recomputed on scale-factor change.
    let font = render::font_from_str(&plan.font);
    let metrics = render::Metrics::measure(&drawing.pango_context(), font);
    let (cell_w, cell_h) = (metrics.cell_w, metrics.cell_h);

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
        last_liveness: now,
        last_metrics_flush: now,
        pending_resize: None,
        resize_deadline: None,
    }));

    {
        let fe = fe.clone();
        drawing.set_draw_func(move |area, cr, w, h| {
            let fe = fe.borrow();
            render::render(&fe.core, cr, &area.pango_context(), &fe.metrics, w, h);
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

    let window = ApplicationWindow::builder()
        .application(app)
        .title("kmux")
        .default_width((80.0 * cell_w) as i32)
        .default_height((24.0 * cell_h) as i32)
        .build();
    window.set_child(Some(&drawing));

    // Key input: GDK → agnostic key → resolve → dispatch (or structured forward).
    let key_ctl = EventControllerKey::new();
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        let app = app.clone();
        key_ctl.connect_key_pressed(move |_ctl, keyval, _code, gdk_mods| {
            let Some((key, mods)) = convert::convert(keyval, gdk_mods) else {
                return glib::Propagation::Proceed;
            };
            // Resolve + dispatch under a scoped borrow, then drop it before
            // handling the effect (reconnect/switch re-borrow `fe`).
            let result = {
                let mut fe = fe.borrow_mut();
                let (new_mode, action) = mode::resolve(&fe.core.mode, &key, mods);
                if let Some(m) = new_mode {
                    fe.core.mode = m;
                }
                if matches!(action, Action::ForwardKey) {
                    // Snap to bottom on keypress, then forward as a structured
                    // event so the daemon's Ghostty encoder emits the right
                    // bytes under the live terminal mode state.
                    if let Some(grid) = fe.core.mgr.active_grid_mut() {
                        grid.scroll_to_bottom();
                    }
                    if let Some(proto) = convert::convert_to_protocol_key(keyval, gdk_mods) {
                        fe.core.mgr.send_key_batch(vec![proto]);
                    }
                    KeyResult::Continue
                } else {
                    // dispatch_action is async but performs no awaits; block_on
                    // resolves it immediately without touching the runtime.
                    futures::executor::block_on(fe.core.dispatch_action(action))
                }
            };
            handle_effect(&fe, result, &app, &drawing);
            drawing.queue_draw();
            glib::Propagation::Stop
        });
    }
    window.add_controller(key_ctl);

    // The pump: drain network channels, tick timers, request a redraw.
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        glib::timeout_add_local(PUMP_INTERVAL, move || {
            pump(&fe, &drawing);
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

    window.present();
}

/// One pump tick. Mirrors the arms of the TUI `tokio::select!` loop: settled
/// resize, server messages, bootstrap outcome, transport upgrade, tunnel death,
/// liveness, and metrics flush.
fn pump(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    let mut fe = fe.borrow_mut();
    let mut dirty = false;
    let now = Instant::now();

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
                    .enter_disconnected(DisconnectReason::BootstrapFailed("cancelled".to_string()));
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

    if dirty || fe.core.needs_render {
        fe.core.needs_render = false;
        drawing.queue_draw();
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
