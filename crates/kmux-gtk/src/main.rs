//! GTK4 frontend scaffold for kmux.
//!
//! Proof-of-seam, not a finished GUI. It demonstrates that the toolkit-agnostic
//! [`kmux_app::core::AppCore`] can drive a native GTK frontend exactly as it
//! drives the TUI: a glib main-loop *pump* polls the core's network channels
//! and forwards input; a `DrawingArea` renders `AppCore`'s active grid; GDK key
//! events are converted to the shared key model and fed through
//! `mode::resolve` → `AppCore::dispatch_action`.
//!
//! `AppCore` is *driven, not driving* here just as in the TUI — only the pump
//! and the render leaf are GTK-specific.

mod convert;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, DrawingArea, EventControllerKey, cairo, gdk, glib};

use kmux_app::core::{AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult};
use kmux_app::launch::{Launch, Plan, run_cli};
use kmux_app::mode::{self, Action};
use kmux_client::generate_instance_id;
use kmux_protocol::messages::{ClientCapabilities, ServerMessage, TermSize};
use tokio::sync::mpsc;

const APP_ID: &str = "dev.getkono.kmux";
// Cell metrics (must match the monospace font size below). Mirrors the
// server-side CELL_WIDTH/CELL_HEIGHT used for pixel geometry.
const CELL_W: f64 = 8.0;
const CELL_H: f64 = 16.0;
const FONT_SIZE: f64 = 13.0;

/// Shared frontend state pumped by the glib loop. The receivers are the same
/// channels the TUI's event loop owns; here the glib pump drains them instead
/// of a `tokio::select!`.
struct Frontend {
    core: AppCore,
    srv_rx: mpsc::UnboundedReceiver<ServerMessage>,
    bootstrap_rx: mpsc::UnboundedReceiver<BootstrapTaskResult>,
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
            run_gui(plan)
        }
    }
}

/// Run the GTK application for an interactive session built from `plan`.
fn run_gui(plan: Plan) -> anyhow::Result<()> {
    let app = Application::builder().application_id(APP_ID).build();
    let plan = std::rc::Rc::new(plan);
    app.connect_activate(move |app| build_ui(app, &plan));
    app.run();
    Ok(())
}

fn build_ui(app: &Application, plan: &Plan) {
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

    // The frontend owns the server-message + bootstrap channels (as the TUI's
    // run loop does) and kicks off the initial bootstrap.
    let (srv_tx, srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (bs_tx, bootstrap_rx) = mpsc::unbounded_channel::<BootstrapTaskResult>();
    if let Some(target) = core.pending_target.take() {
        core.start_bootstrap(target, srv_tx, BootstrapPhase::Initial, bs_tx);
    }

    let fe = Rc::new(RefCell::new(Frontend {
        core,
        srv_rx,
        bootstrap_rx,
    }));

    let drawing = DrawingArea::new();
    drawing.set_hexpand(true);
    drawing.set_vexpand(true);
    {
        let fe = fe.clone();
        drawing.set_draw_func(move |_area, cr, w, h| {
            render(&fe.borrow().core, cr, w, h);
        });
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("kmux")
        .default_width(80 * CELL_W as i32)
        .default_height(24 * CELL_H as i32)
        .build();
    window.set_child(Some(&drawing));

    // Key input: GDK → agnostic key → resolve → dispatch (or raw-forward).
    let key_ctl = EventControllerKey::new();
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        let app = app.clone();
        key_ctl.connect_key_pressed(move |_ctl, keyval, _code, gdk_mods| {
            let Some((key, mods)) = convert::convert(keyval, gdk_mods) else {
                return glib::Propagation::Proceed;
            };
            let mut fe = fe.borrow_mut();
            let (new_mode, action) = mode::resolve(&fe.core.mode, &key, mods);
            if let Some(m) = new_mode {
                fe.core.mode = m;
            }
            if matches!(action, Action::ForwardKey) {
                if let Some(grid) = fe.core.mgr.active_grid_mut() {
                    grid.scroll_to_bottom();
                }
                let bytes = convert::forward_bytes(&key, mods);
                if !bytes.is_empty() {
                    fe.core.mgr.send_input(bytes);
                }
            } else {
                // dispatch_action is async but performs no awaits; block_on
                // resolves it immediately without touching the tokio runtime.
                let result = futures::executor::block_on(fe.core.dispatch_action(action));
                handle_effect(result, &app, &drawing);
            }
            drawing.queue_draw();
            glib::Propagation::Stop
        });
    }
    window.add_controller(key_ctl);

    // The pump: drain network channels, update the core, request a redraw.
    {
        let fe = fe.clone();
        let drawing = drawing.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            pump(&fe, &drawing);
            glib::ControlFlow::Continue
        });
    }

    window.present();
}

/// One pump tick: keep the core's term size in sync with the window, drain the
/// server-message and bootstrap channels, and queue a redraw on change.
fn pump(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    let mut fe = fe.borrow_mut();
    let mut dirty = false;

    // Sync content size from the widget (cols/rows from pixel geometry).
    let cols = (drawing.width() as f64 / CELL_W).floor().max(1.0) as u16;
    let rows = (drawing.height() as f64 / CELL_H).floor().max(1.0) as u16;
    if cols != fe.core.term_size.cols || rows != fe.core.term_size.rows {
        let size = TermSize {
            rows,
            cols,
            pixel_width: drawing.width() as u16,
            pixel_height: drawing.height() as u16,
        };
        fe.core.set_term_size(size);
        dirty = true;
    }

    // Drain server messages.
    while let Ok(msg) = fe.srv_rx.try_recv() {
        let events = fe.core.mgr.handle_server_message(msg);
        fe.core.handle_session_events(events);
        dirty = true;
    }

    // Drain bootstrap outcomes (local daemon: no SSH supervisor to launch).
    while let Ok(outcome) = fe.bootstrap_rx.try_recv() {
        match outcome {
            BootstrapTaskResult::Success(o) => {
                let _ = fe.core.mgr.apply_outcome(*o);
                fe.core.reflect_bootstrap_outcome();
            }
            BootstrapTaskResult::Failed(reason) => {
                use kmux_client::connection_state::DisconnectReason;
                fe.core
                    .enter_disconnected(DisconnectReason::BootstrapFailed(reason));
            }
        }
        dirty = true;
    }

    if dirty || fe.core.needs_render {
        fe.core.needs_render = false;
        drawing.queue_draw();
    }
}

/// Perform the toolkit-specific follow-up for a dispatch result. Scaffold-grade:
/// handles Quit and clipboard copy; reconnect/switch/paste are TUI-only for now.
fn handle_effect(result: KeyResult, app: &Application, drawing: &DrawingArea) {
    match result {
        KeyResult::Quit => app.quit(),
        KeyResult::CopyToClipboard(text) => {
            if let Some(display) = gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
        }
        // RequestPaste / Reconnect / SwitchServer: not wired in the scaffold.
        _ => {}
    }
    drawing.queue_draw();
}

/// Render the active grid (proof-of-seam; not optimized — repaints every cell).
fn render(core: &AppCore, cr: &cairo::Context, _w: i32, _h: i32) {
    let bg = core.palette.bg;
    cr.set_source_rgb(
        bg.r as f64 / 255.0,
        bg.g as f64 / 255.0,
        bg.b as f64 / 255.0,
    );
    let _ = cr.paint();

    cr.select_font_face(
        "monospace",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_font_size(FONT_SIZE);

    let Some(grid) = core.mgr.active_grid() else {
        let fg = core.palette.fg;
        cr.set_source_rgb(
            fg.r as f64 / 255.0,
            fg.g as f64 / 255.0,
            fg.b as f64 / 255.0,
        );
        cr.move_to(16.0, 28.0);
        let _ = cr.show_text("kmux — connecting to local daemon…");
        return;
    };

    let cells = grid.cells();
    let cols = grid.cols;
    for row in 0..grid.rows {
        for col in 0..cols {
            let Some(cell) = cells.get(row * cols + col) else {
                continue;
            };
            let x = col as f64 * CELL_W;
            let y = row as f64 * CELL_H;

            cr.set_source_rgb(
                cell.bg.r as f64 / 255.0,
                cell.bg.g as f64 / 255.0,
                cell.bg.b as f64 / 255.0,
            );
            cr.rectangle(x, y, CELL_W, CELL_H);
            let _ = cr.fill();

            if cell.c != ' ' && cell.c != '\0' {
                cr.set_source_rgb(
                    cell.fg.r as f64 / 255.0,
                    cell.fg.g as f64 / 255.0,
                    cell.fg.b as f64 / 255.0,
                );
                cr.move_to(x, y + CELL_H - 3.0);
                let _ = cr.show_text(&cell.c.to_string());
            }
        }
    }

    let cur = grid.cursor();
    if cur.visible {
        let fg = core.palette.fg;
        cr.set_source_rgba(
            fg.r as f64 / 255.0,
            fg.g as f64 / 255.0,
            fg.b as f64 / 255.0,
            0.6,
        );
        cr.rectangle(
            cur.col as f64 * CELL_W,
            cur.row as f64 * CELL_H,
            CELL_W,
            CELL_H,
        );
        let _ = cr.fill();
    }
}
