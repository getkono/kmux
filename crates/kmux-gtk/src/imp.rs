//! GTK4 frontend implementation (Linux + macOS).
//!
//! ## Platform gating
//!
//! The GTK4 + libadwaita stack runs on Linux (system packages) and macOS
//! (Homebrew: `brew install gtk4 libadwaita`), so this entire module is gated to
//! those targets **once**, at the `mod imp;` declaration in `main.rs` (the GTK
//! crates are target-gated to match in `Cargo.toml`). Nothing inside here needs a
//! per-item `#[cfg]`. On other targets `main.rs` compiles only a stub `main`.
//! Linux is the default + official target; on macOS the GTK frontend is an
//! alternative to the native SwiftUI app (`kmux-swift`).
//!
//! ## Architecture
//!
//! The toolkit-agnostic [`kmux_app::driver::FrontendDriver`] owns the run-loop
//! orchestration (network channels, bootstrap, liveness, metrics, resize
//! debounce, cursor blink) that used to live inline here. This frontend is now
//! just the GTK leaves around it: a glib timeout *pump* calls
//! [`FrontendDriver::tick`] each frame and acts on the returned
//! [`FrontendEffect`]s, a `DrawingArea` renders the driver's active grid, and
//! GDK key events are converted to the shared key model and forwarded to the
//! PTY (window/app accelerators bind straight to `Action`s; see `actions.rs`).
//!
//! `AppCore` is *driven, not driving* — only the pump cadence and the
//! render/input leaves are GTK-specific. The same `FrontendDriver` is what a
//! non-Rust frontend (e.g. the SwiftUI macOS app, via `kmux-ffi`) drives too.

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
mod tiles;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::{Application, DrawingArea, EventControllerKey, gdk, gio, glib};

use kmux_app::core::AppCore;
use kmux_app::driver::{FrontendDriver, FrontendEffect};
use kmux_app::launch::{Launch, Plan, run_cli};
use kmux_app::mode::Mode;
use kmux_app::theme::Theme;
use kmux_client::generate_instance_id;
use kmux_protocol::messages::{ClientCapabilities, TermSize};

const APP_ID: &str = "dev.getkono.kmux";

/// Pump cadence: drain the driver + tick timers (~60 Hz).
const PUMP_INTERVAL: Duration = Duration::from_millis(16);

/// Shared frontend state. The toolkit-agnostic run loop lives in `core`
/// ([`FrontendDriver`], which wraps `AppCore`); the GTK leaves keep only the
/// render geometry and the chrome CSS provider.
pub(crate) struct Frontend {
    /// The toolkit-agnostic driver wrapping `AppCore`. Named `core` so the GTK
    /// modules reach `AppCore` state through it via `Deref` (`f.core.mgr`,
    /// `f.core.mode`, `f.core.palette`, …) exactly as before.
    core: FrontendDriver,
    /// Cell geometry derived from the configured font; recomputed on scale
    /// change. Drives the grid render and the resize → cols/rows mapping.
    metrics: render::Metrics,
    /// The CSS provider for the chrome/overlay theme, reloaded when the driver
    /// reports a palette change (`/theme`).
    css_provider: gtk4::CssProvider,
}

/// Entry point for the interactive GTK frontend. Shares the CLI front door with
/// the other frontends, then runs the GTK application for an interactive launch.
pub(crate) fn run() -> anyhow::Result<()> {
    // A tokio runtime backs the driver's async orchestration (start_bootstrap
    // spawns tasks) and the CLI front door's daemon/subcommand network calls.
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
    let core = AppCore::new(
        plan.target.clone(),
        plan.initial_cwd.clone(),
        plan.instance_id.clone(),
        plan.auto_session.clone(),
        plan.auto_cwd.clone(),
        capabilities,
        plan.theme.clone(),
        plan.cursor_blink,
        term_size,
    );

    // The driver owns the network channels and kicks off the initial bootstrap
    // (from `core.pending_target`). We are inside the tokio runtime (entered in
    // `main`), so its `start_bootstrap` spawn lands on it.
    let driver = FrontendDriver::new(core);

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

    let fe = Rc::new(RefCell::new(Frontend {
        core: driver,
        metrics,
        css_provider,
    }));

    {
        let fe = fe.clone();
        drawing.set_draw_func(move |area, cr, w, h| {
            let fe = fe.borrow();
            render::render_tiled(
                &fe.core,
                cr,
                &area.pango_context(),
                &fe.metrics,
                w,
                h,
                fe.core.blink_on(),
            );
        });
    }

    // Window-resize → debounced term-size update. Event-driven (like the TUI's
    // SIGWINCH) rather than polled; the driver applies it once the burst settles.
    {
        let fe = fe.clone();
        drawing.connect_resize(move |_area, w, h| {
            let mut fe = fe.borrow_mut();
            let (cols, rows) = fe.metrics.cols_rows(w, h);
            fe.core.request_resize(TermSize {
                rows,
                cols,
                pixel_width: w.max(0) as u16,
                pixel_height: h.max(0) as u16,
            });
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
        shell.banner.connect_button_clicked(move |_| {
            fe.borrow_mut().core.reconnect();
            shell2.drawing.queue_draw();
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
            f.core.scroll_to_bottom();
            if let Some(proto) = convert::convert_to_protocol_key(keyval, gdk_mods) {
                // `send_keys` forwards to the PTY and restarts the blink cycle so
                // typing shows a solid cursor.
                f.core.send_keys(vec![proto]);
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

    // The pump: tick the driver, apply effects, sync the shell/dialogs, redraw.
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

/// One pump tick: advance the driver, apply the toolkit-specific effects it
/// returns, and — if anything changed — reconcile the native shell + overlays
/// and repaint the grid.
fn pump(
    fe: &Rc<RefCell<Frontend>>,
    shell: &Rc<shell::Shell>,
    dialogs: &Rc<dialogs::Dialogs>,
    app: &Application,
) {
    let effects = fe.borrow_mut().core.tick();
    let redraw = apply_effects(fe, effects, app, &shell.drawing);
    if redraw {
        header::sync(shell, fe);
        tabs::sync(shell, fe);
        sidebar::sync(shell, fe);
        dialogs::sync(dialogs, shell, fe, app);
        // Keep each visible pane's PTY sized to its resolved tile (no-op when no
        // tile's size changed). Skipped until the drawing has been allocated.
        let (w, h) = (shell.drawing.width(), shell.drawing.height());
        if w > 0 && h > 0 {
            tiles::push_sizes(fe, w, h);
        }
        shell.drawing.queue_draw();
    }
}

/// Perform the toolkit-specific follow-up for a batch of [`FrontendEffect`]s
/// (from [`FrontendDriver::tick`] or an input dispatch). Returns whether a
/// repaint is needed. Reconnect / server-switch are handled inside the driver
/// and never reach here.
pub(crate) fn apply_effects(
    fe: &Rc<RefCell<Frontend>>,
    effects: Vec<FrontendEffect>,
    app: &Application,
    drawing: &DrawingArea,
) -> bool {
    let mut redraw = false;
    for eff in effects {
        match eff {
            FrontendEffect::NeedsRender | FrontendEffect::ForceClear => redraw = true,
            FrontendEffect::PaletteChanged => {
                // Reflect a `/theme` palette change onto the chrome CSS + window
                // light/dark styling (the cairo grid reads the palette live).
                let f = fe.borrow();
                css::reload(&f.css_provider, &f.core.palette);
                let scheme = scheme_for(&f.core.palette);
                drop(f);
                adw::StyleManager::default().set_color_scheme(scheme);
                redraw = true;
            }
            FrontendEffect::CopyToClipboard(text) => copy_to_clipboard(&text),
            FrontendEffect::RequestPaste => request_paste(fe, drawing),
            FrontendEffect::Quit => app.quit(),
        }
    }
    redraw
}

/// Write `text` to the system clipboard. The driver already strips interior NUL
/// bytes (which would make `Clipboard::set_text` — a non-unwinding FFI
/// trampoline — abort the process), so we can write the payload directly.
fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

/// Read the system clipboard asynchronously and feed it back as a paste once it
/// lands (clipboard reads are async in GTK).
fn request_paste(fe: &Rc<RefCell<Frontend>>, drawing: &DrawingArea) {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let fe = fe.clone();
    let drawing = drawing.clone();
    display
        .clipboard()
        .read_text_async(gio::Cancellable::NONE, move |res| {
            if let Ok(Some(text)) = res {
                fe.borrow_mut().core.feed_paste(text.to_string());
                drawing.queue_draw();
            }
        });
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
