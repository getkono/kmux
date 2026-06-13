//! `AppCore`: the frontend-agnostic client view-model and orchestration state.
//!
//! `AppCore` holds the session manager plus all interaction/connection state
//! that is independent of any UI toolkit. Frontends (`kmux-gtk`, `kmux-swift`)
//! *drive* it: they pump input in (keys, actions, resize, server messages) and
//! read state out for rendering. `AppCore` never owns the run loop, a terminal,
//! or a widget — it is a passive state machine plus orchestration methods.
//!
//! Toolkit-specific state (color types, `Rect` hit-boxes, the clipboard
//! channel) lives on the frontend's own struct, not here.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::session_manager::SessionManager;
use kmux_client::ssh::RemoteTarget;
use kmux_protocol::messages::{ClientCapabilities, ServerMessage, TermSize};
use tokio::sync::{mpsc, oneshot};

use crate::appearance::Appearance;
use crate::mode::Mode;
use crate::recent_servers::{RecentServersCache, ServerKind};
use crate::theme::Theme;

mod connection_info;
mod dispatch;
mod orchestration;

pub use connection_info::{ConnectionInfo, RttInfo, TransportTraffic};
pub use orchestration::{BootstrapPhase, BootstrapTaskResult};

/// Maximum entries kept in [`AppCore::command_history`].
pub const COMMAND_HISTORY_CAP: usize = 100;

/// Rolling window over which the HUD's rendering-FPS counter is measured (#61).
const RENDER_FPS_WINDOW: Duration = Duration::from_secs(1);
/// Grace period between requesting a pane close and actually killing the shell
/// (issue #86). A healthy pane's `PaneClose` is withheld for this long so an
/// accidental close can be undone within the window.
pub const SOFT_CLOSE_GRACE: Duration = Duration::from_secs(3);

/// A pane whose close has been requested but deferred (issue #86). The real
/// `PaneClose` is sent only once [`deadline`](Self::deadline) passes; until then
/// the user can cancel (undo), leaving the live shell untouched.
#[derive(Debug, Clone)]
pub struct PendingClose {
    pub pane_id: String,
    pub deadline: Instant,
}

/// What a key/action dispatch returns to the frontend's run loop.
///
/// This is the core → frontend control-flow channel: the frontend matches on
/// it and performs the toolkit-specific follow-up (replace the server-message
/// channel, exit the loop, …).
pub enum KeyResult {
    Continue,
    Quit,
    /// User submitted the Connect form; the frontend must replace `srv_rx`.
    Reconnect,
    /// User selected a server from the server picker.
    SwitchServer(SwitchTarget),
    /// Core requests the frontend copy this text to the system clipboard.
    /// Clipboard access is toolkit-specific, so it is performed frontend-side.
    CopyToClipboard(String),
    /// Core requests the frontend read the system clipboard and feed it back as
    /// a paste (the frontend forwards the text to `mgr.send_paste`).
    RequestPaste,
}

/// Destination chosen from the server picker.
#[derive(Debug)]
pub enum SwitchTarget {
    Local,
    Ssh(RemoteTarget),
}

/// Action carried by a clickable top-bar segment. Frontend-neutral intent: a
/// GUI binds these to widgets for click handling. The hit-testing geometry
/// itself stays frontend-side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopBarAction {
    OpenServerPicker,
    Reconnect,
    OpenSessionPicker,
    SelectPane(String),
    /// Affordance to spawn a new pane in the active session.
    CreatePane,
}

/// One row in the directory browser (the "new session — choose a directory"
/// overlay). Frontends render these rows and dispatch the row's effect on
/// activation; the row order is fixed by [`AppCore::dir_browser_rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirBrowserRow {
    /// Create a new session in `cwd` (the currently-browsed directory). Row 0.
    CreateHere { cwd: String },
    /// Navigate up to the parent directory. Present only when the listing has a
    /// parent (absent at the filesystem root).
    Up { parent: String },
    /// Navigate into the subdirectory `path` (display name `name`).
    Enter { path: String, name: String },
}

/// Frontend-agnostic client view-model. See the module docs.
pub struct AppCore {
    pub mgr: SessionManager,

    /// Active color palette (toolkit-neutral). The source of truth that the
    /// `/theme` command mutates; each frontend converts it to its own color
    /// type at the render boundary. Named `palette` (not `theme`) so it does
    /// not shadow a frontend's own rendered-theme field through a deref.
    pub palette: Theme,

    /// Active terminal appearance (font family/size/style, OpenType features,
    /// cell adjustments). Toolkit-neutral, like [`palette`](Self::palette): each
    /// frontend converts it to its own font/metrics types at the render leaf.
    /// Resolved from `config.toml` at construction.
    pub appearance: Appearance,

    /// Whether the inner-pane cursor blinks. When `false`, a cursor that
    /// requested blinking (DECSCUSR `blinking_*` / DEC mode 12) is drawn steady.
    /// The blink phase is driven by the frontend pump
    /// ([`crate::driver::blink`]); this gates whether it advances at all.
    pub cursor_blink_enabled: bool,

    /// Current interaction mode (modal keymap state).
    pub mode: Mode,

    /// Last terminal/content size reported by the frontend. Used when creating
    /// sessions/panes and for auto-select. The frontend keeps this current via
    /// [`AppCore::set_term_size`].
    pub term_size: TermSize,

    pub hud_visible: bool,
    pub metrics_overlay_visible: bool,
    /// Whether the connection inspector overlay is open (issue #60). Like the
    /// metrics overlay, this is a passive flag the frontends reconcile against.
    pub connection_overlay_visible: bool,
    pub force_snapshot_mode: bool,

    /// Whether the HUD shows the network-latency + rendering-FPS counters
    /// (issue #61). When `false`, the counters are hidden *and* their per-frame
    /// calculation is skipped (power-efficient). Resolved from `config.toml`.
    pub show_perf_counters: bool,
  
    /// Recent render timestamps (most-recent last) within [`RENDER_FPS_WINDOW`],
    /// used to compute the rendering FPS. Only populated while the HUD is shown
    /// and the counters are enabled, so it costs nothing otherwise.
    render_frames: VecDeque<Instant>,
  
    /// Panes pending a deferred (soft) close (issue #86), oldest first. While a
    /// pane is here its `PaneClose` has NOT been sent; the driver fires it once
    /// the deadline passes, and the user can undo within the window.
    pub pending_closes: Vec<PendingClose>,
  
    /// Bumped on every soft-close request so a frontend can show its "Undo"
    /// affordance exactly once per scheduled close (not every frame).
    pub soft_close_nonce: u64,

    /// Reconnection bookkeeping.
    pub disconnect_at: Option<Instant>,

    // Session picker state (logical: search query + selected index).
    pub session_picker_selected: usize,
    pub session_picker_search: String,

    // Directory browser state (choose where to open a new session). The browser
    // lists the daemon host's directories over the protocol (so it works for a
    // remote daemon). `dir_picker_buffer` is the per-row **filter** text and
    // `dir_picker_selected` is the highlighted row index; the entries/parent
    // come from `mgr.dir_listing()`.
    pub dir_picker_buffer: String,
    pub dir_picker_selected: usize,
    /// The directory currently being browsed (the listing request target).
    pub dir_browser_cwd: String,

    // Auto-session selection context.
    pub is_local: bool,
    pub initial_cwd: String,
    pub did_auto_select: bool,
    /// CLI `--session` flag: auto-attach to a session by name or word_id.
    pub auto_session: Option<String>,
    /// Effective cwd from `--cwd` or `:path` in server string.
    pub auto_cwd: Option<String>,

    /// Human-readable label for the current server shown in the server badge.
    pub server_display: String,
    /// Cache key for the current server (empty for local).
    pub server_string: String,
    /// Connection kind for the current server (used for reconnect routing).
    pub server_kind: ServerKind,

    // Server picker state.
    pub server_picker_selected: usize,
    pub server_picker_search: String,

    /// Persisted recent-servers cache.
    pub recent_servers: RecentServersCache,

    /// Frontend should schedule a frame.
    pub needs_render: bool,
    /// Frontend should perform a full repaint (clear + redraw).
    pub force_clear: bool,

    /// Drop to cancel an in-progress background bootstrap.
    pub cancel_tx: Option<oneshot::Sender<()>>,
    /// Clone of the server-message sender for the active bootstrap channel.
    /// Used by `launch_ssh_supervisor` after a successful SSH bootstrap.
    pub pending_srv_tx: Option<mpsc::UnboundedSender<ServerMessage>>,

    /// Unique ID for this client process, written to the connection log on auth.
    pub instance_id: String,
    /// SSH target stored for re-negotiation when the tunnel dies (SSH only).
    pub ssh_target: Option<RemoteTarget>,
    /// Target for the initial bootstrap. Consumed on the first connect.
    pub pending_target: Option<ResolvedTarget>,

    /// Last fatal error to surface *after* the frontend has torn down, e.g. a
    /// bootstrap failure on a fresh launch. The frontend reads this on exit.
    pub last_exit_error: Option<String>,

    /// Most-recently-submitted command-palette buffers (oldest first), capped
    /// at [`COMMAND_HISTORY_CAP`].
    pub command_history: VecDeque<String>,
}

impl AppCore {
    /// Build the core view-model for a target. `capabilities` is detected by
    /// the frontend (it is terminal/toolkit-specific) and `term_size` is the
    /// frontend's initial content size.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target: ResolvedTarget,
        initial_cwd: String,
        instance_id: String,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
        capabilities: ClientCapabilities,
        theme: Theme,
        appearance: Appearance,
        cursor_blink: bool,
        term_size: TermSize,
    ) -> Self {
        let (is_local, accept_invalid_certs, ssh_target) = match &target {
            ResolvedTarget::LocalDaemon => (true, true, None),
            ResolvedTarget::Ssh {
                target,
                accept_invalid_certs,
            } => (false, *accept_invalid_certs, Some(target.clone())),
        };

        let (server_display, server_string, server_kind) = if is_local {
            ("localhost".to_string(), String::new(), ServerKind::Local)
        } else {
            let t = ssh_target
                .as_ref()
                .expect("ssh_target must be Some when not local");
            let display = match &t.user {
                Some(u) => format!("{}@{}", u, t.host),
                None => t.host.clone(),
            };
            let kind = ServerKind::Ssh {
                user: t.user.clone(),
                host: t.host.clone(),
                ssh_port: t.ssh_port,
            };
            (display.clone(), display, kind)
        };

        // Seed SessionManager with placeholder host/port/token. `apply_outcome`
        // overwrites these when the bootstrap completes.
        let mut mgr = SessionManager::new(
            "127.0.0.1".to_string(),
            0,
            String::new(),
            accept_invalid_certs,
            capabilities,
        );
        mgr.enable_metrics_persistence();
        // Seed the initial size so the first Attach carries real dimensions.
        mgr.update_term_size(term_size);

        Self {
            mgr,
            palette: theme,
            appearance,
            cursor_blink_enabled: cursor_blink,
            mode: Mode::Normal,
            term_size,
            // Auto-show the performance HUD on debug builds so live diagnostics
            // are on by default during development (#105). Release builds start
            // hidden; either profile can toggle it at runtime (`hud` / ⌘⇧H).
            hud_visible: cfg!(debug_assertions),
            metrics_overlay_visible: false,
            connection_overlay_visible: false,
            force_snapshot_mode: false,
            show_perf_counters: crate::config::resolve_perf_counters(),
            render_frames: VecDeque::new(),
            pending_closes: Vec::new(),
            soft_close_nonce: 0,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            dir_browser_cwd: String::new(),
            is_local,
            initial_cwd,
            did_auto_select: false,
            auto_session,
            auto_cwd,
            server_display,
            server_string,
            server_kind,
            server_picker_selected: 0,
            server_picker_search: String::new(),
            recent_servers: RecentServersCache::load(),
            needs_render: true,
            force_clear: false,
            cancel_tx: None,
            pending_srv_tx: None,
            instance_id,
            ssh_target,
            pending_target: Some(target),
            last_exit_error: None,
            command_history: VecDeque::new(),
        }
    }

    /// Report the current content size from the frontend. Updates the cached
    /// size and forwards it to the session manager (which messages the server).
    pub fn set_term_size(&mut self, size: TermSize) {
        self.term_size = size;
        self.mgr.update_term_size(size);
    }

    // ── Performance counters (issue #61) ──────────────────────────────────────

    /// Record a pump frame for the rendering-FPS counter. `did_render` is whether
    /// a real *content* repaint happened this frame — the HUD's own 60 Hz
    /// self-refresh passes `false` so it does not inflate the rate. Zero-cost
    /// (and clears any history) when the counters are hidden or the HUD is
    /// closed, so it is safe on the pump's hot path.
    pub fn note_render(&mut self, now: Instant, did_render: bool) {
        if !self.show_perf_counters || !self.hud_visible {
            if !self.render_frames.is_empty() {
                self.render_frames.clear();
            }
            return;
        }
        if !did_render {
            return;
        }
        let cutoff = now.checked_sub(RENDER_FPS_WINDOW).unwrap_or(now);
        while self.render_frames.front().is_some_and(|t| *t < cutoff) {
            self.render_frames.pop_front();
        }
        self.render_frames.push_back(now);
    }

    /// Rendering frames per second over the last [`RENDER_FPS_WINDOW`]. Reflects
    /// actual repaints (gated by `needs_render`), so it idles near 0 and peaks at
    /// the ~60 Hz pump cap.
    pub fn render_fps(&self) -> u32 {
        let now = Instant::now();
        let cutoff = now.checked_sub(RENDER_FPS_WINDOW).unwrap_or(now);
        self.render_frames.iter().filter(|t| **t >= cutoff).count() as u32
    }

    /// The latest network round-trip latency (ms) for the active transport.
    /// `None` before the first ping round-trip.
    pub fn net_latency_ms(&self) -> Option<f64> {
        self.mgr.last_rtt_ms()
    }

    /// Whether the latency counter should show its "stale" star — the link has
    /// gone quiet for over 3× the ping interval.
    pub fn net_latency_stale(&self) -> bool {
        self.mgr.is_ping_stale(Instant::now())
    }

    /// Test-only constructor: a local `AppCore` wrapping `mgr` with default
    /// state. Lets frontend command-palette / hint tests build a controlled
    /// instance without booting the runtime or going through `new`.
    #[doc(hidden)]
    pub fn for_test(mgr: SessionManager) -> Self {
        Self {
            mgr,
            palette: crate::theme::default_theme(),
            appearance: Appearance::default(),
            cursor_blink_enabled: true,
            mode: Mode::Normal,
            term_size: TermSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            hud_visible: false,
            metrics_overlay_visible: false,
            connection_overlay_visible: false,
            force_snapshot_mode: false,
            show_perf_counters: true,
            render_frames: VecDeque::new(),
            pending_closes: Vec::new(),
            soft_close_nonce: 0,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            dir_browser_cwd: String::new(),
            is_local: true,
            initial_cwd: String::new(),
            did_auto_select: false,
            auto_session: None,
            auto_cwd: None,
            server_display: String::new(),
            server_string: String::new(),
            server_kind: ServerKind::Local,
            server_picker_selected: 0,
            server_picker_search: String::new(),
            recent_servers: RecentServersCache::load(),
            needs_render: true,
            force_clear: false,
            cancel_tx: None,
            pending_srv_tx: None,
            instance_id: String::new(),
            ssh_target: None,
            pending_target: None,
            last_exit_error: None,
            command_history: VecDeque::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_local_core() -> AppCore {
        AppCore::new(
            ResolvedTarget::LocalDaemon,
            String::new(),
            String::new(),
            None,
            None,
            ClientCapabilities::default(),
            crate::theme::default_theme(),
            Appearance::default(),
            true,
            TermSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        )
    }

    /// The performance HUD auto-shows on debug builds and stays hidden on
    /// release builds (#105). The default is wired to the compile profile, so
    /// it must track `cfg!(debug_assertions)` rather than a hardcoded value.
    #[test]
    fn hud_default_follows_build_profile() {
        assert_eq!(new_local_core().hud_visible, cfg!(debug_assertions));
    }

    /// FPS counts only real content repaints — a HUD-only self-refresh
    /// (`did_render = false`) must not inflate it (issue #61).
    #[test]
    fn render_fps_counts_only_content_repaints() {
        let mut core = new_local_core();
        core.show_perf_counters = true;
        core.hud_visible = true;
        let t = Instant::now();
        core.note_render(t, false); // HUD self-refresh — ignored
        assert_eq!(core.render_fps(), 0);
        core.note_render(t, true);
        core.note_render(t, true);
        assert_eq!(core.render_fps(), 2);
    }

    /// With the counters hidden, `note_render` is inert (no calculation), which
    /// is what makes the config toggle power-efficient (issue #61).
    #[test]
    fn note_render_is_inert_when_counters_hidden() {
        let mut core = new_local_core();
        core.hud_visible = true;
        core.show_perf_counters = false;
        core.note_render(Instant::now(), true);
        assert_eq!(core.render_fps(), 0);
    }
}
