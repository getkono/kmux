//! kmux-tui — the terminal (ratatui/crossterm) frontend for kmux.
//!
//! **Deprecated.** The GTK GUI (`kmux`, from the `kmux-gtk` crate) is the
//! primary client and has reached feature parity. `kmux-tui` is retained for
//! SSH/headless and no-display use; it stays compiling and tested (it is the
//! regression oracle for the shared `kmux-app` interaction layer) but is no
//! longer the focus of new feature work.

mod app;
mod key_convert;
mod theme;
mod ui;

// Frontend-free logic lives in kmux-app; re-export the bits the app/ui modules
// reach via `crate::*`.
use kmux_app::{cmd, host_caps, mode};

use std::io;

use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::prelude::CrosstermBackend;
use tracing::Instrument;

use app::App;
use kmux_app::launch::{Launch, Plan, run_cli};
use kmux_client::generate_instance_id;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let instance_id = generate_instance_id();
    match run_cli(instance_id).await? {
        Launch::Done => Ok(()),
        Launch::Interactive(plan) => run_tui(*plan).await,
    }
}

/// Drive the terminal frontend for an interactive session: set up the terminal,
/// build the `App` from the launch plan, run the event loop, and restore.
async fn run_tui(plan: Plan) -> anyhow::Result<()> {
    let instance_id = plan.instance_id.clone();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Try to enable the kitty keyboard protocol on the host terminal so
    // crossterm sees Shift+Enter, Alt+Enter, Shift+Tab, etc. as
    // distinguishable events rather than collapsing them into the bare
    // key.  Terminals that don't support it ignore the push and we fall
    // back to legacy behaviour.
    let kitty_kbd_supported = supports_keyboard_enhancement().unwrap_or(false);
    if kitty_kbd_supported {
        // Disambiguate is essential.  Alternate keys help kitty-aware apps.
        // We deliberately do NOT enable REPORT_EVENT_TYPES (release events
        // would double-fire keystrokes) or REPORT_ALL_KEYS_AS_ESCAPE_CODES
        // (would break plain typing in legacy code paths).
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
            )
        );
    }

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    // Install panic hook to restore terminal — pop kitty flags BEFORE leaving
    // the alt screen so the host terminal returns to its baseline state.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        if kitty_kbd_supported {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        original_hook(panic_info);
    }));

    let mut app = App::new(
        plan.target,
        plan.initial_cwd,
        plan.theme,
        plan.instance_id,
        plan.auto_session,
        plan.auto_cwd,
        kitty_kbd_supported,
    );

    let result = app
        .run(&mut terminal)
        .instrument(tracing::info_span!("instance", id = %instance_id))
        .await;

    // Restore terminal
    disable_raw_mode()?;
    if kitty_kbd_supported {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // The TUI alternate-screen overlay swallows error text on exit. If the
    // bootstrap (e.g. SSH negotiation) failed, the App stashed the full
    // multi-line diagnostic for us to surface here, after raw-mode is off.
    if let Some(err) = app.last_exit_error.take() {
        eprintln!("kmux: connection failed:\n{err}");
    }

    result
}
