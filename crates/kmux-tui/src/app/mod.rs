use std::ops::{Deref, DerefMut, Range};

use kmux_client::pipeline::ResolvedTarget;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::theme::Theme;

// The frontend-agnostic view-model and the types the frontend shares with it
// now live in kmux-app. Re-export them at `crate::app::*` so existing call
// sites (event loop, key/mouse handlers, command palette) keep resolving.
pub use kmux_app::core::{
    AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult, SwitchTarget, TopBarAction,
};

mod event_batch;
mod event_loop;
mod helpers;
mod input_coalesce;
mod key_handler;
mod mouse_handler;

/// Column ranges recorded by `render_session_bar` so the mouse handler can
/// hit-test clicks on row 0 without duplicating render-layout math. Stored in
/// left-to-right order — purely display, mouse logic just scans for the first
/// range containing the cursor.
#[derive(Default)]
pub struct TopBarHits {
    pub regions: Vec<(Range<u16>, TopBarAction)>,
}

impl TopBarHits {
    pub fn action_at(&self, col: u16) -> Option<&TopBarAction> {
        self.regions
            .iter()
            .find(|(r, _)| r.contains(&col))
            .map(|(_, a)| a)
    }
}

/// Rects recorded by the active picker overlay so the mouse handler can
/// support hover-to-highlight, click-to-select, and outside-click dismissal.
#[derive(Default)]
pub struct PickerHits {
    pub rect: Option<Rect>,
    /// Absolute screen row of each rendered item, in filtered-list order.
    pub item_rows: Vec<u16>,
}

/// The TUI application: a thin presentation wrapper around the shared
/// [`AppCore`] view-model, adding the ratatui / crossterm-specific state (the
/// rendered color theme, mouse hit-boxes, the clipboard channel).
///
/// `App` derefs to `AppCore` so the event loop and renderers reach core state
/// (`self.mgr`, `self.mode`, …) and orchestration methods directly — a
/// deliberate newtype-wrapper ergonomic. The frontend's own fields shadow
/// nothing on the core: the core's agnostic palette is named `palette`, and
/// this `theme` is its ratatui-typed mirror. A native GUI frontend wraps the
/// same `AppCore` the same way.
pub struct App {
    pub core: AppCore,

    /// The active palette as ratatui colors — the rendered mirror of
    /// `core.palette`, refreshed before each draw (see `run`).
    pub theme: Theme,

    /// Clickable regions on the top bar (row 0), refreshed every render.
    pub top_bar_hits: TopBarHits,
    /// Clickable regions for the currently rendered picker overlay, refreshed
    /// every render. Empty when no picker is open.
    pub picker_hits: PickerHits,

    /// Sender side of the paste channel; wired up by `App::run`. Clipboard
    /// reads run on a blocking thread and the result arrives here.
    pub(super) paste_tx: Option<mpsc::UnboundedSender<String>>,
}

impl Deref for App {
    type Target = AppCore;
    fn deref(&self) -> &AppCore {
        &self.core
    }
}

impl DerefMut for App {
    fn deref_mut(&mut self) -> &mut AppCore {
        &mut self.core
    }
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: ResolvedTarget,
        initial_cwd: String,
        theme: kmux_app::theme::Theme,
        instance_id: String,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
        kitty_keyboard_supported: bool,
    ) -> Self {
        let capabilities = crate::host_caps::detect(kitty_keyboard_supported);
        // The core owns the agnostic palette; the TUI keeps a ratatui-typed
        // mirror (refreshed before each draw — see `run`).
        let tui_theme = Theme::from(theme.clone());
        let core = AppCore::new(
            target,
            initial_cwd,
            instance_id,
            auto_session,
            auto_cwd,
            capabilities,
            theme,
            Self::current_term_size(),
        );

        Self {
            core,
            theme: tui_theme,
            top_bar_hits: TopBarHits::default(),
            picker_hits: PickerHits::default(),
            paste_tx: None,
        }
    }
}
