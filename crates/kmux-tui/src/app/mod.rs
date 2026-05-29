use std::ops::{Deref, DerefMut, Range};

use kmux_client::pipeline::ResolvedTarget;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::theme::Theme;

// The frontend-agnostic view-model and the types the frontend shares with it
// now live in kmux-app. Re-export them at `crate::app::*` so existing call
// sites (event loop, key/mouse handlers, command palette) keep resolving.
pub use kmux_app::core::{
    AppCore, BootstrapPhase, BootstrapTaskResult, COMMAND_HISTORY_CAP, KeyResult, SwitchTarget,
    TopBarAction,
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

/// The TUI application: the shared [`AppCore`] view-model plus the ratatui /
/// crossterm-specific presentation state (color palette, mouse hit-boxes, the
/// clipboard channel).
///
/// `App` derefs to `AppCore` so the event loop, command palette, and renderers
/// reach core state (`self.mgr`, `self.mode`, …) and orchestration methods
/// transparently. This bridge is intentionally temporary — P6 replaces it with
/// explicit `self.core.*` access once the dispatch/command logic has also moved
/// into `AppCore`.
pub struct App {
    pub core: AppCore,

    /// Color palette as ratatui colors (converted from the agnostic `kmux_app`
    /// palette at the `main.rs` boundary).
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
        theme: Theme,
        instance_id: String,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
        kitty_keyboard_supported: bool,
    ) -> Self {
        let capabilities = crate::host_caps::detect(kitty_keyboard_supported);
        let core = AppCore::new(
            target,
            initial_cwd,
            instance_id,
            auto_session,
            auto_cwd,
            capabilities,
            Self::current_term_size(),
        );

        Self {
            core,
            theme,
            top_bar_hits: TopBarHits::default(),
            picker_hits: PickerHits::default(),
            paste_tx: None,
        }
    }
}
