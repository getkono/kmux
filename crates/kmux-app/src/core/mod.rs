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

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::session_manager::SessionManager;
use kmux_client::ssh::RemoteTarget;
use kmux_protocol::messages::{ClientCapabilities, PeerId, PeerTarget, ServerMessage, TermSize};
use tokio::sync::{mpsc, oneshot};

use crate::appearance::Appearance;
use crate::mode::Mode;
use crate::recent_servers::{RecentServersCache, ServerKind};
use crate::theme::Theme;

mod clients;
mod connection_info;
mod dispatch;
mod orchestration;
mod overview;
mod render_debug;

pub use connection_info::{ConnectionInfo, RttInfo, TransportTraffic};
pub use orchestration::{BootstrapPhase, BootstrapTaskResult};
pub use overview::{OverviewRow, OverviewRowKind, build_overview_rows};
pub use render_debug::{CursorDebug, PaneDebug, RenderDebugSnapshot};

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

/// A session whose close has been requested but deferred (issue #64). The real
/// `SessionClose` is sent only once `deadline` passes; until then the user can
/// undo, leaving the live session (and all its processes) untouched.
#[derive(Debug, Clone)]
pub struct PendingSessionClose {
    pub word_id: String,
    /// Display name captured at request time, for the "restored" toast.
    pub name: String,
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
    /// Diagnostic: the frontend must rebuild its renderer + glyph atlas (it owns
    /// that object) and force a full repaint. See [`crate::mode::Action::ResetRenderer`].
    ResetRenderer,
    /// Core requests the frontend copy this text to the system clipboard.
    /// Clipboard access is toolkit-specific, so it is performed frontend-side.
    CopyToClipboard(String),
    /// Core requests the frontend read the system clipboard and feed it back as
    /// a paste (the frontend forwards the text to `mgr.send_paste`).
    RequestPaste,
}

/// Action carried by a clickable top-bar segment. Frontend-neutral intent: a
/// GUI binds these to widgets for click handling. The hit-testing geometry
/// itself stays frontend-side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopBarAction {
    Reconnect,
    OpenSessionPicker,
    /// Open the unified session launcher (issue #121): the new-session button.
    OpenLaunchPicker,
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

/// Connection status of a remote in the launcher (issue #121). Drives the
/// status indicator on a remote's row; `Error` carries the reason for inline
/// display and keeps the failure isolated to that one remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    /// Known but not connected this session.
    Idle,
    /// An `OpenPeer` is in flight (expanded, awaiting `PeerOpened`).
    Connecting,
    /// Federated: the peer's sessions are live in the merged list.
    Connected,
    /// The last connect attempt failed; the string is the reason, shown inline.
    Error(String),
}

/// One row in the unified session launcher (issue #121): a flat,
/// frontend-renderable projection of "open or create a session, locally or on a
/// remote". A dumb frontend renders each row and dispatches its activation; the
/// order and contents are fixed by [`AppCore::launch_rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchRow {
    /// Open a new local session, defaulting to `default_cwd` (the focused
    /// session's root). Activating opens the directory browser seeded there.
    LocalNewSession { default_cwd: String },
    /// Attach an existing local session.
    LocalExisting {
        word_id: String,
        name: String,
        cwd: String,
        active: bool,
    },
    /// A remote's header/toggle row. Activating expands it (connecting on focus)
    /// or collapses it. `status` drives the indicator; `expanded` the chevron.
    Remote {
        peer: PeerId,
        label: String,
        status: RemoteStatus,
        expanded: bool,
    },
    /// Open a new session on `peer` (shown only while the remote is expanded and
    /// connected). Activating opens a path prompt.
    RemoteNewSession { peer: PeerId },
    /// Attach an existing session on `peer` (shown only while expanded).
    RemoteExisting {
        peer: PeerId,
        word_id: String,
        name: String,
        cwd: String,
        active: bool,
    },
    /// Restore a closed (inactive) local session from the daemon's graveyard
    /// (issue #64). Shown in a "Restore" section, ordered most-recently-active
    /// first. Activating respawns it and attaches.
    ClosedSession {
        word_id: String,
        name: String,
        cwd: String,
        /// Epoch-ms of last activity before close; frontends render it as a
        /// relative "last active" label and the rows are ordered by it.
        last_active_ms: u64,
    },
    /// Affordance to add a new remote (SSH or Direct). Always the last row.
    AddRemote,
}

/// Format an epoch-ms "last active" timestamp as a short relative label for the
/// restore UI (issue #64): `"just now"`, `"5m ago"`, `"2h ago"`, `"3d ago"`.
/// `0` (the "unknown" sentinel for pre-v4 sessions) yields `"unknown"`.
/// Frontend-agnostic so GTK and the Swift FFI render closed-session rows alike.
pub fn relative_time_label(last_active_ms: u64) -> String {
    if last_active_ms == 0 {
        return "unknown".to_string();
    }
    let now = kmux_protocol::messages::epoch_millis();
    let secs = now.saturating_sub(last_active_ms) / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Values collected by the add-remote form (issue #121). The frontend owns the
/// native input widgets and hands a completed form to
/// [`AppCore::submit_add_remote`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddRemoteForm {
    /// `true` for an SSH remote (the default), `false` for a `Direct` TCP+TLS one.
    pub use_ssh: bool,
    /// Hostname / IP / SSH alias. Required.
    pub host: String,
    /// SSH user; empty means "default user" (SSH only).
    pub user: String,
    /// SSH port override, or the `Direct` TCP+TLS port (required for `Direct`).
    pub port: Option<u16>,
    /// Shared token for a `Direct` peer (required; never persisted to disk).
    pub token: String,
    /// Accept a self-signed / unpinned server certificate.
    pub accept_invalid_certs: bool,
}

/// Why the connection is paused, surfaced to frontends for a status indicator
/// (issue #68).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// Not paused.
    None,
    /// Paused by an explicit user toggle.
    Manual,
    /// Auto-paused because the app window is backgrounded/minimized.
    Auto,
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
    /// Whether the render-debug overlay is shown — the diagnostic that exposes
    /// what the renderer is handed each frame (cursor logical + pixel geometry,
    /// renderer leaf, scene counts). A passive flag the frontends reconcile.
    pub render_debug_visible: bool,
    pub force_snapshot_mode: bool,

    /// Connection paused by an explicit user toggle (issue #68). Persists across
    /// window focus changes — only another toggle clears it.
    pub manual_pause: bool,
    /// Connection auto-paused because the app window is backgrounded/minimized
    /// (issue #68). Cleared automatically when the window returns to foreground.
    pub auto_pause: bool,

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

    /// Sessions pending a deferred (soft) close (issue #64), oldest first. Like
    /// `pending_closes` but for whole sessions: the `SessionClose` is withheld
    /// for [`SOFT_CLOSE_GRACE`] so an accidental close can be undone instantly
    /// (the live session is never touched); after the window it is closed and
    /// becomes restorable from the daemon's graveyard.
    pub pending_session_closes: Vec<PendingSessionClose>,

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
    /// `(program, args)` to run in a fresh dedicated initial session instead of
    /// a shell. Set by `kmux diagnostic <test>` (issue #145); `None` otherwise.
    /// Consumed once by [`auto_select_session`](Self::auto_select_session).
    pub initial_program: Option<(String, Vec<String>)>,

    /// Human-readable label for the current server shown in the server badge.
    pub server_display: String,
    /// Cache key for the current server (empty for local).
    pub server_string: String,
    /// Connection kind for the current server (used for reconnect routing).
    pub server_kind: ServerKind,

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
    /// SSH target stored for display/identity of the current remote server.
    pub ssh_target: Option<RemoteTarget>,
    /// The remote peer this GUI wants federated through the local daemon (issue
    /// #121). `None` means a purely local server. When set, the GUI bootstraps
    /// the **local** daemon (always UDS) and issues `OpenPeer` after every
    /// successful (re)connect, so the remote network stack lives in `kmuxd`, not
    /// here. Replaces the old "GUI opens its own SSH connection" model.
    pub desired_peer: Option<PeerTarget>,
    /// Target for the initial bootstrap. Consumed on the first connect. Always
    /// the local daemon now; a remote `--server` is conveyed via `desired_peer`.
    pub pending_target: Option<ResolvedTarget>,

    /// Last fatal error to surface *after* the frontend has torn down, e.g. a
    /// bootstrap failure on a fresh launch. The frontend reads this on exit.
    pub last_exit_error: Option<String>,

    /// Most-recently-submitted command-palette buffers (oldest first), capped
    /// at [`COMMAND_HISTORY_CAP`].
    pub command_history: VecDeque<String>,

    // ──── Unified session launcher (issue #121) ────
    /// Highlighted row in the launcher (index into [`AppCore::launch_rows`]).
    pub launch_selected: usize,
    /// Launcher filter text (matches local/remote session names + remote hosts).
    pub launch_search: String,
    /// Remotes whose section is expanded — expanding connects on focus, so this
    /// is also "the remotes the user has focused this session".
    pub launch_expanded: HashSet<PeerId>,
    /// Per-remote connection status, keyed by [`PeerId`]. Drives each remote
    /// row's indicator; a failed connect lands here (isolated) instead of
    /// tearing down the whole UI.
    pub peer_status: HashMap<PeerId, RemoteStatus>,
    /// In-session remote registry: the [`PeerTarget`] used to (re)connect each
    /// known remote, keyed by [`PeerId`]. Seeded from recents (SSH) and the CLI
    /// `--server` peer, extended by the add-remote form (incl. `Direct`, whose
    /// token lives only here). The source of truth for connect and for
    /// re-federation after a reconnect.
    pub peer_targets: HashMap<PeerId, PeerTarget>,
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
        initial_program: Option<(String, Vec<String>)>,
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

        // Federation model (issue #121): a remote `--server` no longer opens its
        // own SSH/TLS connection from the GUI. Instead the GUI always bootstraps
        // the local daemon (UDS) and asks it to federate the remote via
        // `OpenPeer`. `is_local` still reflects *server identity* (it drives
        // auto-select), while the bootstrap target is always the local daemon.
        let desired_peer = match &target {
            ResolvedTarget::LocalDaemon => None,
            ResolvedTarget::Ssh {
                target,
                accept_invalid_certs,
            } => Some(PeerTarget::Ssh {
                user: target.user.clone(),
                host: target.host.clone(),
                ssh_port: target.ssh_port,
                accept_invalid_certs: *accept_invalid_certs,
            }),
        };
        let bootstrap_target = if desired_peer.is_some() {
            ResolvedTarget::LocalDaemon
        } else {
            target
        };
        // Suppress the auto-select that the *pre-federation* session list would
        // trigger; it is re-armed when `PeerOpened` arrives so the federated
        // sessions are what the picker/auto-select acts on.
        let suppress_initial_auto_select = desired_peer.is_some();

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

        // Seed the launcher's remote registry from remembered servers (SSH only)
        // plus the CLI `--server` peer, so known remotes appear and a reconnect
        // can re-federate them. The `--server` peer starts expanded (it is the
        // server the user explicitly asked for).
        let recent_servers = RecentServersCache::load();
        let mut peer_targets: HashMap<PeerId, PeerTarget> = HashMap::new();
        for srv in recent_servers.servers() {
            if let (Some(id), Some(target)) = (srv.kind.peer_id(), srv.kind.to_peer_target(false)) {
                peer_targets.entry(id).or_insert(target);
            }
        }
        let mut launch_expanded: HashSet<PeerId> = HashSet::new();
        if let Some(target) = &desired_peer {
            let id = target.peer_id();
            peer_targets.insert(id.clone(), target.clone());
            launch_expanded.insert(id);
        }

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
            render_debug_visible: false,
            force_snapshot_mode: false,
            manual_pause: false,
            auto_pause: false,
            show_perf_counters: crate::config::resolve_perf_counters(),
            render_frames: VecDeque::new(),
            pending_closes: Vec::new(),
            pending_session_closes: Vec::new(),
            soft_close_nonce: 0,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            dir_browser_cwd: String::new(),
            is_local,
            initial_cwd,
            did_auto_select: suppress_initial_auto_select,
            auto_session,
            auto_cwd,
            initial_program,
            server_display,
            server_string,
            server_kind,
            recent_servers,
            needs_render: true,
            force_clear: false,
            cancel_tx: None,
            pending_srv_tx: None,
            instance_id,
            ssh_target,
            desired_peer,
            pending_target: Some(bootstrap_target),
            last_exit_error: None,
            command_history: VecDeque::new(),
            launch_selected: 0,
            launch_search: String::new(),
            launch_expanded,
            peer_status: HashMap::new(),
            peer_targets,
        }
    }

    /// Report the current content size from the frontend. Updates the cached
    /// size and forwards it to the session manager (which messages the server).
    pub fn set_term_size(&mut self, size: TermSize) {
        self.term_size = size;
        self.mgr.update_term_size(size);
    }

    // ── Connection pause (issue #68) ──────────────────────────────────────────

    /// Effective pause state: paused if either the user toggled it or the app
    /// is auto-paused while backgrounded.
    pub fn is_paused(&self) -> bool {
        self.manual_pause || self.auto_pause
    }

    /// Why the connection is currently paused, for chrome indicators.
    pub fn pause_reason(&self) -> PauseReason {
        if self.manual_pause {
            PauseReason::Manual
        } else if self.auto_pause {
            PauseReason::Auto
        } else {
            PauseReason::None
        }
    }

    /// Whether terminal output for `pane_id` is currently withheld (issue #68):
    /// a manual pause withholds every pane; a background auto-pause withholds all
    /// but panes marked exempt. Drives the per-pane + per-tab pause indicators.
    pub fn is_pane_paused(&self, pane_id: &str) -> bool {
        self.manual_pause || (self.auto_pause && !self.mgr.is_pane_auto_pause_exempt(pane_id))
    }

    /// The pause reason as it applies to a single pane (per-pane chrome).
    pub fn pane_pause_reason(&self, pane_id: &str) -> PauseReason {
        if self.manual_pause {
            PauseReason::Manual
        } else if self.auto_pause && !self.mgr.is_pane_auto_pause_exempt(pane_id) {
            PauseReason::Auto
        } else {
            PauseReason::None
        }
    }

    /// Whether `pane_id` is marked exempt from auto-pause at the *pane* level
    /// (drives the pane menu's checkmark; issue #68).
    pub fn pane_no_auto_pause(&self, pane_id: &str) -> bool {
        self.mgr.pane_marked_auto_pause_exempt(pane_id)
    }

    /// Whether `word_id` is marked exempt from auto-pause at the *session* level
    /// (session/tab menu checkmark; issue #68).
    pub fn session_no_auto_pause(&self, word_id: &str) -> bool {
        self.mgr.session_marked_auto_pause_exempt(word_id)
    }

    /// Reconcile the effective pause state (and per-pane exemptions) with the
    /// connection. Idempotent — the session manager sends `SetPaused` only on
    /// change and re-attaches exactly the panes that resume streaming.
    fn reconcile_pause(&mut self) {
        let paused = self.is_paused();
        let auto = matches!(self.pause_reason(), PauseReason::Auto);
        self.mgr.reconcile_pause(paused, auto);
    }

    /// Flip the manual pause toggle (the `TogglePause` action). A manual pause
    /// persists across window focus changes.
    pub fn toggle_manual_pause(&mut self) {
        self.manual_pause = !self.manual_pause;
        self.reconcile_pause();
    }

    /// Set the auto-pause flag (driven by window background/foreground, with a
    /// debounce in the driver). Independent of the manual toggle.
    pub fn set_auto_pause(&mut self, on: bool) {
        if self.auto_pause == on {
            return;
        }
        self.auto_pause = on;
        self.reconcile_pause();
    }

    /// Toggle `pane_id`'s exemption from auto-pause (issue #68): an exempt pane
    /// keeps streaming through a background auto-pause.
    pub fn toggle_pane_no_auto_pause(&mut self, pane_id: &str) {
        self.mgr.toggle_pane_auto_pause_exempt(pane_id);
        self.reconcile_pause();
    }

    /// Toggle the *focused* pane's exemption from auto-pause (the menu/keyboard
    /// action). No-op without a focused pane.
    pub fn toggle_focused_pane_no_auto_pause(&mut self) {
        if let Some(pane_id) = self.mgr.active_pane.clone() {
            self.toggle_pane_no_auto_pause(&pane_id);
        }
    }

    /// Toggle a whole session's exemption from auto-pause (issue #68); every pane
    /// in the session inherits it.
    pub fn toggle_session_no_auto_pause(&mut self, word_id: &str) {
        self.mgr.toggle_session_auto_pause_exempt(word_id);
        self.reconcile_pause();
    }

    /// Toggle the *active* session's exemption from auto-pause (the menu action).
    /// No-op without an active session.
    pub fn toggle_active_session_no_auto_pause(&mut self) {
        if let Some(word_id) = self.mgr.active_session.clone() {
            self.toggle_session_no_auto_pause(&word_id);
        }
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
            render_debug_visible: false,
            force_snapshot_mode: false,
            manual_pause: false,
            auto_pause: false,
            show_perf_counters: true,
            render_frames: VecDeque::new(),
            pending_closes: Vec::new(),
            pending_session_closes: Vec::new(),
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
            initial_program: None,
            server_display: String::new(),
            server_string: String::new(),
            server_kind: ServerKind::Local,
            recent_servers: RecentServersCache::load(),
            needs_render: true,
            force_clear: false,
            cancel_tx: None,
            pending_srv_tx: None,
            instance_id: String::new(),
            ssh_target: None,
            desired_peer: None,
            pending_target: None,
            last_exit_error: None,
            command_history: VecDeque::new(),
            launch_selected: 0,
            launch_search: String::new(),
            launch_expanded: HashSet::new(),
            peer_status: HashMap::new(),
            peer_targets: HashMap::new(),
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

    /// Pause has two independent sources (issue #68): a manual toggle and an
    /// auto-pause while backgrounded. The effective state is their OR, a manual
    /// pause persists across focus changes, and the reason favours Manual.
    #[test]
    fn pause_state_machine_manual_persists_across_focus() {
        let mut core = new_local_core();
        assert!(!core.is_paused());
        assert_eq!(core.pause_reason(), PauseReason::None);

        // Window backgrounded → auto-pause.
        core.set_auto_pause(true);
        assert!(core.is_paused());
        assert_eq!(core.pause_reason(), PauseReason::Auto);

        // User also toggles a manual pause.
        core.toggle_manual_pause();
        assert!(core.is_paused());
        assert_eq!(core.pause_reason(), PauseReason::Manual);

        // Window returns to foreground: auto clears, but the manual pause stays.
        core.set_auto_pause(false);
        assert!(core.is_paused(), "manual pause must survive foregrounding");
        assert_eq!(core.pause_reason(), PauseReason::Manual);

        // User toggles the manual pause off → fully resumed.
        core.toggle_manual_pause();
        assert!(!core.is_paused());
        assert_eq!(core.pause_reason(), PauseReason::None);
    }

    /// Per-pane pause indicators honor an auto-pause exemption, but a manual
    /// pause overrides it (issue #68).
    #[test]
    fn per_pane_pause_respects_auto_pause_exemption() {
        let mut core = new_local_core();
        core.toggle_pane_no_auto_pause("w/0");
        assert!(core.pane_no_auto_pause("w/0"));

        // Auto-pause: the exempt pane keeps streaming; its siblings are paused.
        core.set_auto_pause(true);
        assert!(
            !core.is_pane_paused("w/0"),
            "exempt pane streams under auto-pause"
        );
        assert_eq!(core.pane_pause_reason("w/0"), PauseReason::None);
        assert!(core.is_pane_paused("w/1"), "non-exempt pane is auto-paused");
        assert_eq!(core.pane_pause_reason("w/1"), PauseReason::Auto);

        // Manual pause overrides the exemption: every pane is paused.
        core.toggle_manual_pause();
        assert!(
            core.is_pane_paused("w/0"),
            "manual pause overrides the exemption"
        );
        assert_eq!(core.pane_pause_reason("w/0"), PauseReason::Manual);
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
