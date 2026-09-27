//! [`crate::driver::FrontendDriver`]: the toolkit-agnostic run-loop orchestration shared by
//! every frontend.
//!
//! [`crate::core::AppCore`] is a passive state machine; *driving* it has always meant the
//! same arm-for-arm loop — own the four network channels, drain server messages,
//! settle a debounced resize, handle the bootstrap outcome (and launch the SSH
//! supervisor), apply a transport upgrade, react to a tunnel death, tick the
//! liveness ping + metrics flush, and advance the cursor blink. That loop is not
//! UI-specific, yet it used to live inside each frontend (e.g. the `kmux-gtk`
//! glib `pump`), duplicated and — for a non-Rust frontend reaching `AppCore`
//! across an FFI boundary — impossible to express in the target language.
//!
//! `FrontendDriver` lifts that orchestration here. A frontend now:
//!
//! - builds an [`crate::core::AppCore`] with its own capabilities, wraps it with
//!   [`crate::driver::FrontendDriver::new`] (which creates the channels and kicks off the
//!   initial bootstrap),
//! - calls [`crate::driver::FrontendDriver::tick`] once per frame from its own loop (a glib
//!   timeout, a `CVDisplayLink`, …) and acts on the returned [`crate::driver::FrontendEffect`]s
//!   (repaint, copy to clipboard, request paste, quit),
//! - feeds input in via [`dispatch_action`](crate::driver::FrontendDriver::dispatch_action),
//!   [`send_keys`](crate::driver::FrontendDriver::send_keys), [`request_resize`], the picker
//!   drivers, …,
//! - reads state out via [`Deref`](std::ops::Deref) to [`crate::core::AppCore`] (`driver.mgr`, `driver.mode`,
//!   `driver.palette`, …) plus [`active_grid`](crate::driver::FrontendDriver::active_grid) and
//!   [`blink_on`](crate::driver::FrontendDriver::blink_on).
//!
//! It owns no run loop and no runtime: it assumes an *ambient* tokio runtime
//! (the spawning paths use the current `Handle`) exactly as the frontends do
//! today, so the caller stays in control of the loop and the runtime.
//!
//! [`request_resize`]: crate::driver::FrontendDriver::request_resize

mod blink;
mod clipboard;
mod frame_trace;
mod reconnect;

pub use blink::{CURSOR_BLINK_HALF, advance_blink};
pub use clipboard::sanitize_clipboard_text;
pub use reconnect::{
    ConnectionBanner, DROPPED_NOTICE, OUTAGE_INPUT_CAPACITY, UNREACHABLE_AFTER_ATTEMPTS,
};

use self::reconnect::{OutageInput, Reconnect, connection_banner};

use std::collections::HashMap;
use std::ops::Deref;
use std::time::{Duration, Instant};

use kmux_client::connection_state::DisconnectReason;
use kmux_client::grid::CellGrid;
#[cfg(feature = "remote")]
use kmux_client::supervisor::UpgradeSignal;
#[cfg(feature = "remote")]
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{
    AttentionKind, ClientMessage, KeyEvent, PaneId, ServerMessage, TermSize, epoch_millis,
};
use kmux_protocol::trace::{AppliedDiff, ClientTickRecord};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use self::frame_trace::ClientTraceSink;

/// Logical-frame coalescing window (issue #72). Cell diffs the daemon emitted
/// within this many ms of each other are considered one logical frame; painting
/// part of such a group across separate pump ticks is a tear. Overridable via
/// `KMUX_TEAR_WINDOW_MS`.
const TEAR_WINDOW_MS: u64 = 16;
/// Minimum cell ops for a diff to count as logical-frame content — filters
/// single-cell keystroke echoes and cursor blinks (cursor-only updates are
/// `CursorUpdate`, already excluded). Overridable via `KMUX_TEAR_MIN_OPS`.
const TEAR_MIN_OPS: usize = 4;

/// Decide whether the previous paint showed a partial logical frame: true when
/// the previously-painted cell diff and this tick's earliest qualifying cell
/// diff were emitted by the daemon within `window_ms` of each other (so they
/// belonged to one logical frame but were painted across two ticks).
pub(crate) fn tear_detected(
    prev_painted_sent_at_ms: Option<u64>,
    tick_first_sent_at_ms: u64,
    window_ms: u64,
) -> bool {
    match prev_painted_sent_at_ms {
        Some(prev) => tick_first_sent_at_ms
            .checked_sub(prev)
            .is_some_and(|gap| gap < window_ms),
        None => false,
    }
}

/// Read a `u64` env override, falling back to `default` when unset/invalid.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

use crate::core::{AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult};
use crate::mode::{Action, Mode};

/// Liveness ping + timeout evaluation cadence.
const LIVENESS_TICK: Duration = Duration::from_secs(1);
/// Metrics JSONL flush cadence (see `docs/metrics.md`).
const METRICS_FLUSH_TICK: Duration = Duration::from_secs(10);
/// Process-overview refresh cadence while the overview is open (issue #122).
/// Matched to the daemon's lazy-sample interval so CPU deltas stay meaningful.
const PROCESS_OVERVIEW_TICK: Duration = Duration::from_secs(1);
/// Connected-clients refresh cadence while that view is open (issue #146), so
/// the list reflects clients attaching/detaching without a manual reopen.
const CONNECTED_CLIENTS_TICK: Duration = Duration::from_secs(1);
/// Debounce window for resize bursts; a window drag fires many size changes, so
/// coalesce them into one `set_term_size` after the burst settles.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

/// How long the window must stay backgrounded before the connection
/// auto-pauses (issue #68). Short enough to save bandwidth promptly, long
/// enough to ride out a quick alt-tab without thrashing pause/resume.
const AUTO_PAUSE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// What a [`FrontendDriver::tick`] (or input dispatch) asks the frontend to do.
///
/// This is the driver → frontend channel for the few actions that are inherently
/// toolkit-specific. Everything else (reconnect, server switch, channel rebuilds,
/// SSH supervisor launch, transport upgrade, tunnel death, liveness/metrics,
/// bootstrap outcome) is handled *inside* the driver and never surfaces here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontendEffect {
    /// Schedule a repaint of the grid (and reconcile native chrome).
    NeedsRender,
    /// Perform a full repaint (clear + redraw).
    ForceClear,
    /// Diagnostic: rebuild the frontend's renderer + glyph atlas, then repaint.
    /// The renderer object is frontend-owned, so only the frontend can recreate
    /// it — hence this dedicated effect (see [`crate::mode::Action::ResetRenderer`]).
    ResetRenderer,
    /// The `/theme` palette changed; reload toolkit-specific chrome styling.
    /// Implies a repaint. Read the new palette from [`crate::core::AppCore::palette`].
    PaletteChanged,
    /// Copy this (already NUL-sanitized) text to the system clipboard.
    CopyToClipboard(String),
    /// Read the system clipboard and feed it back via [`FrontendDriver::feed_paste`].
    RequestPaste,
    /// Exit the application.
    Quit,
    /// A program in a pane requested attention via `kmux notify` (issue #169).
    /// The frontend raises a native desktop notification and, on click,
    /// refocuses the window for `word_id` + selects `pane_id`. `attention_id` is
    /// unique per request so a frontend dedups to one notification across its
    /// windows. See `docs/architecture-claude-integration.md`.
    Attention {
        word_id: String,
        pane_id: String,
        kind: AttentionKind,
        title: String,
        body: String,
        attention_id: u64,
    },
}

/// Toolkit-agnostic run-loop driver wrapping an [`AppCore`]. See the module docs.
pub struct FrontendDriver {
    core: AppCore,
    /// Server messages for the live connection. Replaced on reconnect / server
    /// switch (the old receiver is dropped, closing the stale channel).
    srv_rx: mpsc::UnboundedReceiver<ServerMessage>,
    /// Outcome channel for the in-flight bootstrap; `None` while idle.
    bootstrap_rx: Option<mpsc::UnboundedReceiver<BootstrapTaskResult>>,
    /// Better-transport signals from the background supervisor probe. The sender
    /// lives for the whole session; clones are handed to the SSH supervisor.
    /// Only a `remote` build dials remotes directly and can upgrade transports;
    /// a lean GUI is always UDS-local, so the whole subsystem is gated out.
    #[cfg(feature = "remote")]
    upgrade_rx: mpsc::Receiver<UpgradeSignal>,
    #[cfg(feature = "remote")]
    upgrade_tx: mpsc::Sender<UpgradeSignal>,
    /// SSH tunnel-death signal (the tunnel process exited unexpectedly).
    #[cfg(feature = "remote")]
    tunnel_died_rx: mpsc::Receiver<()>,
    #[cfg(feature = "remote")]
    tunnel_died_tx: mpsc::Sender<()>,
    /// Last palette applied, for `/theme` change detection.
    last_palette: crate::theme::Theme,
    /// Per-cadence bookkeeping: the pump fires on one interval, so each timer
    /// tracks its own last-fire / deadline.
    last_liveness: Instant,
    last_metrics_flush: Instant,
    /// Last time a process-overview snapshot was requested (issue #122). Used to
    /// throttle re-requests to [`PROCESS_OVERVIEW_TICK`] while the view is open.
    last_process_overview: Instant,
    /// Last time the connected-clients list was requested (issue #146). Throttles
    /// re-requests to [`CONNECTED_CLIENTS_TICK`] while that view is open.
    last_connected_clients: Instant,
    pending_resize: Option<TermSize>,
    resize_deadline: Option<Instant>,
    /// Cursor-blink phase: `true` shows the cursor on the current frame.
    blink_on: bool,
    /// When the current blink half-cycle started; reset on keypress so typing
    /// shows a solid cursor.
    blink_phase_start: Instant,
    /// Per-pane `sent_at_ms` of the most recent cell diff painted, for the
    /// tearing detector (issue #72).
    tear_state: HashMap<PaneId, u64>,
    /// Coalescing window + min-ops thresholds for the tearing detector.
    tear_window_ms: u64,
    tear_min_ops: usize,
    /// When the app window first went to the background, if it currently is
    /// (issue #68 auto-pause). After [`AUTO_PAUSE_DEBOUNCE`] still backgrounded,
    /// `tick` auto-pauses the connection; foregrounding clears this immediately.
    background_since: Option<Instant>,
    /// Monotonic pump-tick id, used to tag client frame-trace records.
    tick_id: u64,
    /// Automatic reconnect of a dropped link (issue #208).
    reconnect: Reconnect,
    /// Input typed while the link is down, delivered on reconnect.
    outage: OutageInput,
    /// The banner last shown, so a tick repaints when it changes (a countdown
    /// second, a notice expiring) and not otherwise.
    last_banner: Option<ConnectionBanner>,
    /// Optional per-tick frame-trace sink (`KMUX_FRAME_TRACE`).
    trace: Option<ClientTraceSink>,
}

impl FrontendDriver {
    /// Wrap an [`AppCore`], create the network channels, and kick off the initial
    /// bootstrap from `core.pending_target` (if any).
    ///
    /// Must be called with an ambient tokio runtime (`start_bootstrap` spawns).
    pub fn new(mut core: AppCore) -> Self {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (bs_tx, bs_rx) = mpsc::unbounded_channel::<BootstrapTaskResult>();
        #[cfg(feature = "remote")]
        let (upgrade_tx, upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
        #[cfg(feature = "remote")]
        let (tunnel_died_tx, tunnel_died_rx) = mpsc::channel::<()>(1);

        let bootstrap_rx = if let Some(target) = core.pending_target.take() {
            core.start_bootstrap(target, srv_tx, BootstrapPhase::Initial, bs_tx);
            Some(bs_rx)
        } else {
            None
        };

        let now = Instant::now();
        let last_palette = core.palette.clone();
        Self {
            core,
            srv_rx,
            bootstrap_rx,
            #[cfg(feature = "remote")]
            upgrade_rx,
            #[cfg(feature = "remote")]
            upgrade_tx,
            #[cfg(feature = "remote")]
            tunnel_died_rx,
            #[cfg(feature = "remote")]
            tunnel_died_tx,
            last_palette,
            last_liveness: now,
            last_metrics_flush: now,
            last_process_overview: now,
            last_connected_clients: now,
            pending_resize: None,
            resize_deadline: None,
            blink_on: true,
            blink_phase_start: now,
            tear_state: HashMap::new(),
            tear_window_ms: env_u64("KMUX_TEAR_WINDOW_MS", TEAR_WINDOW_MS),
            tear_min_ops: env_u64("KMUX_TEAR_MIN_OPS", TEAR_MIN_OPS as u64) as usize,
            background_since: None,
            tick_id: 0,
            trace: ClientTraceSink::from_env(),
            reconnect: Reconnect::new(kmux_client::backoff::jitter_seed()),
            outage: OutageInput::default(),
            last_banner: None,
        }
    }

    // ── Pump ────────────────────────────────────────────────────────────────

    /// One non-blocking pump iteration: drain every network channel, tick the
    /// timers, settle a debounced resize, and advance the blink. Returns the
    /// [`FrontendEffect`]s the frontend must act on (at most one trailing
    /// [`FrontendEffect::NeedsRender`] when anything changed).
    ///
    /// The frontend calls this once per frame from its own loop.
    pub fn tick(&mut self) -> Vec<FrontendEffect> {
        self.tick_at(Instant::now())
    }

    /// [`Self::tick`] at `now`.
    fn tick_at(&mut self, now: Instant) -> Vec<FrontendEffect> {
        let mut effects = Vec::new();
        self.tick_id = self.tick_id.wrapping_add(1);

        let palette_changed = self.detect_palette_change();
        if palette_changed {
            effects.push(FrontendEffect::PaletteChanged);
        }
        // Each step, in order, reports whether it changed what is shown.
        let changed = [
            palette_changed,
            self.apply_settled_resize(now),
            // Off-UI-thread grid apply (issue #182, §1): load any content the
            // apply worker republished since last tick, then apply the view
            // effects / resyncs it reported, before draining (and enqueueing)
            // this tick's server messages.
            self.core.mgr.refresh_buffers(),
            self.core.mgr.drain_apply_notes(),
            self.drain_server_messages(&mut effects, now),
            self.poll_bootstrap_outcome(now),
            self.drain_remote(now),
            self.tick_liveness(now),
            // Automatic reconnect (issue #208).
            self.tick_reconnect(now),
            self.refresh_banner(now),
            // Refresh the process overview while it is open (issue #122).
            self.tick_process_overview(now),
            // Refresh the connected-clients list while that view is open (issue #146).
            self.tick_connected_clients(now),
            // Auto-pause the connection once the window has been backgrounded
            // long enough (issue #68).
            self.tick_auto_pause(now),
            // Fire any soft-close whose 3 s grace window has elapsed (issue #86).
            self.core.fire_due_closes(now),
            self.tick_blink(now),
        ];
        self.tick_metrics(now);
        let mut dirty = changed.contains(&true);

        if self.core.take_render_request() {
            dirty = true;
        }
        if self.core.force_clear {
            self.core.force_clear = false;
            effects.push(FrontendEffect::ForceClear);
            dirty = true;
        }

        // Count real content repaints for the rendering-FPS counter (issue #61)
        // BEFORE the HUD's own 60 Hz self-refresh below would inflate the rate.
        self.core.note_render(now, dirty);

        // Keep the live HUD ticker refreshing while it is shown (the metrics
        // dialog is a snapshot taken when it opens, so it needs no per-frame tick).
        if self.core.hud_visible {
            dirty = true;
        }

        if dirty {
            effects.push(FrontendEffect::NeedsRender);
        }
        effects
    }

    /// Reflect a `/theme` palette change. The grid reads the palette live; this
    /// only flags the toolkit-specific chrome reload.
    fn detect_palette_change(&mut self) -> bool {
        if self.core.palette == self.last_palette {
            false
        } else {
            self.last_palette = self.core.palette.clone();
            true
        }
    }

    /// Apply a settled (debounced) resize once its deadline passes.
    fn apply_settled_resize(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        if let Some(deadline) = self.resize_deadline
            && now >= deadline
        {
            if let Some(size) = self.pending_resize.take() {
                self.core.set_term_size(size);
                dirty = true;
            }
            self.resize_deadline = None;
        }
        dirty
    }

    /// Drain batched server messages → session manager → session-event effects
    /// (OSC 52 clipboard writes). Skipped once disconnected; a closed channel
    /// while live means the connection dropped.
    fn drain_server_messages(&mut self, effects: &mut Vec<FrontendEffect>, now: Instant) -> bool {
        if matches!(self.core.mode, Mode::Disconnected { .. }) {
            return false;
        }
        let mut batch = Vec::new();
        let mut closed = false;
        loop {
            match self.srv_rx.try_recv() {
                Ok(m) => batch.push(m),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        let mut dirty = false;
        if !batch.is_empty() {
            self.core.mgr.metrics.record_batch(batch.len());
            // Per-tick diagnostics (issue #72): read each diff's seqno/sent_at/
            // ops before the messages are consumed, so we can run the tearing
            // detector and emit a frame-trace record for this pump tick.
            let (applied, tick_cells) = self.collect_tick_diagnostics(&batch);
            for m in batch {
                let events = self.core.mgr.handle_server_message(m);
                // Server-originated effects — OSC 52 clipboard writes (sanitized
                // in `handle_key_result`) and `kmux notify` attentions (#169) —
                // funnel through the same converter as dispatched key results.
                for eff in self.core.handle_session_events(events) {
                    self.handle_key_result(eff, effects);
                }
            }
            self.detect_tears(tick_cells);
            // A non-empty batch always repaints, so painted = true for the trace.
            if let Some(trace) = self.trace.as_mut()
                && !applied.is_empty()
            {
                trace.record(&ClientTickRecord {
                    tick_id: self.tick_id,
                    at_ms: epoch_millis(),
                    applied,
                    painted: true,
                });
            }
            dirty = true;
        }
        if closed && self.core.mgr.connection_state().is_live() {
            self.link_lost(&DisconnectReason::ServerClosed, now);
            dirty = true;
        }
        dirty
    }

    /// The live link dropped for `reason` at `now`: keep the UI as it is,
    /// hold what the user types, and retry on the backoff schedule (issue
    /// #208).
    fn link_lost(&mut self, reason: &DisconnectReason, now: Instant) {
        tracing::warn!(
            connection_id = self.core.mgr.connection_id.map(|c| c.0),
            transport = %self.core.mgr.current_transport,
            %reason,
            "connection dropped; reconnecting",
        );
        self.reconnect.on_lost(now);
        self.outage.begin();
        self.core.mgr.begin_reconnect_attempt(1);
    }

    /// Whether the banner changed since the last tick (a countdown second
    /// passed, an attempt started, a notice expired) — then the frontend,
    /// which reads the banner when it repaints, must repaint.
    fn refresh_banner(&mut self, now: Instant) -> bool {
        let banner = connection_banner(&self.reconnect, &self.outage, now);
        if banner == self.last_banner {
            return false;
        }
        self.last_banner = banner;
        true
    }

    /// Start the automatic reconnect attempt that is due at `now`, if any.
    fn tick_reconnect(&mut self, now: Instant) -> bool {
        let Some(attempt) = self.reconnect.take_due(now) else {
            return false;
        };
        self.start_bootstrap_with(BootstrapPhase::Resume { attempt });
        true
    }

    /// Rebuild the server + bootstrap channels and bootstrap the current
    /// target for `phase`.
    fn start_bootstrap_with(&mut self, phase: BootstrapPhase) {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel();
        self.srv_rx = srv_rx;
        let (bs_tx, bs_rx) = mpsc::unbounded_channel();
        self.bootstrap_rx = Some(bs_rx);
        let target = self.core.current_target();
        self.core.start_bootstrap(target, srv_tx, phase, bs_tx);
    }

    /// Extract per-diff timing from a drained batch for the tearing detector and
    /// frame trace (issue #72). Returns `(applied, tick_cells)` where `applied`
    /// is every `seqno/sent_at/ops` applied this tick and `tick_cells` is, per
    /// pane, the `(min, max)` `sent_at_ms` over cell diffs with `>= tear_min_ops`
    /// ops (the ones that count as logical-frame content).
    fn collect_tick_diagnostics(
        &self,
        batch: &[ServerMessage],
    ) -> (Vec<AppliedDiff>, HashMap<PaneId, (u64, u64)>) {
        let mut applied: Vec<AppliedDiff> = Vec::new();
        let mut tick_cells: HashMap<PaneId, (u64, u64)> = HashMap::new();
        for m in batch {
            match m {
                ServerMessage::TerminalUpdate {
                    pane_id,
                    diff,
                    seqno,
                    sent_at_ms,
                } => {
                    let ops = diff.ops.len();
                    applied.push(AppliedDiff {
                        seqno: seqno.0,
                        sent_at_ms: *sent_at_ms,
                        ops,
                    });
                    if ops >= self.tear_min_ops {
                        let e = tick_cells
                            .entry(pane_id.clone())
                            .or_insert((*sent_at_ms, *sent_at_ms));
                        e.0 = e.0.min(*sent_at_ms);
                        e.1 = e.1.max(*sent_at_ms);
                    }
                }
                ServerMessage::CursorUpdate {
                    seqno, sent_at_ms, ..
                }
                | ServerMessage::TerminalSnapshot {
                    seqno, sent_at_ms, ..
                }
                | ServerMessage::ScrollbackAppend {
                    seqno, sent_at_ms, ..
                } => {
                    applied.push(AppliedDiff {
                        seqno: seqno.0,
                        sent_at_ms: *sent_at_ms,
                        ops: 0,
                    });
                }
                _ => {}
            }
        }
        (applied, tick_cells)
    }

    /// Run the tearing detector for each pane that applied cell content this
    /// tick, then record this tick's painted state. A tear is counted when the
    /// previous paint's cell diff and this tick's earliest qualifying cell diff
    /// fall within `tear_window_ms` (one logical frame painted across two ticks).
    fn detect_tears(&mut self, tick_cells: HashMap<PaneId, (u64, u64)>) {
        for (pane, (first, last)) in tick_cells {
            let prev = self.tear_state.get(&pane).copied();
            if tear_detected(prev, first, self.tear_window_ms)
                && let Some(prev) = prev
            {
                self.core.mgr.metrics.record_tear(&pane, prev, first);
            }
            self.tear_state.insert(pane, last);
        }
    }

    /// Handle the bootstrap outcome (at most one per bootstrap): wire up the data
    /// plane and launch the SSH supervisor on success, or surface the failure.
    fn poll_bootstrap_outcome(&mut self, now: Instant) -> bool {
        let outcome = self
            .bootstrap_rx
            .as_mut()
            .and_then(|rx| match rx.try_recv() {
                Ok(o) => Some(Ok(o)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(())),
            });
        let Some(outcome) = outcome else {
            return false;
        };
        // The bootstrap is over, whichever way it went.
        self.core.cancel_tx = None;
        self.bootstrap_rx = None;
        match outcome {
            Ok(BootstrapTaskResult::Success(o)) => self.on_bootstrap_success(*o, now),
            Ok(BootstrapTaskResult::Failed(reason)) if self.reconnect.is_active() => {
                self.core.pending_srv_tx = None;
                self.reconnect.on_failed(now, reason);
            }
            Ok(BootstrapTaskResult::Failed(reason) | BootstrapTaskResult::Refused(reason)) => {
                self.on_bootstrap_failure(reason, now);
            }
            // The channel closed with no result: an automatic attempt counts
            // as failed; otherwise the bootstrap was cancelled
            // (`Action::CancelBootstrap` dropped `cancel_tx`).
            Err(()) if self.reconnect.is_active() => {
                self.core.pending_srv_tx = None;
                self.reconnect
                    .on_failed(now, "reconnect attempt cancelled".to_string());
            }
            Err(()) => self.on_bootstrap_cancelled(),
        }
        true
    }

    /// A bootstrap succeeded: wire up the data plane (and the SSH supervisor),
    /// re-federate, and end any outage.
    fn on_bootstrap_success(&mut self, o: kmux_client::pipeline::BootstrapOutcome, now: Instant) {
        // A later success clears any stashed failure so we don't re-print
        // a stale error when the user finally quits.
        self.core.last_exit_error = None;
        let ssh_ctx = self.core.mgr.apply_outcome(o);
        #[cfg(feature = "remote")]
        if let Some(ctx) = ssh_ctx {
            let srv_tx = self
                .core
                .pending_srv_tx
                .take()
                .expect("pending_srv_tx set in start_bootstrap");
            let upgrade_tx = self.upgrade_tx.clone();
            let tunnel_died_tx = self.tunnel_died_tx.clone();
            self.core
                .launch_ssh_supervisor(ctx, srv_tx, upgrade_tx, tunnel_died_tx);
        } else {
            self.core.pending_srv_tx = None;
        }
        // Lean build: the bootstrap is always UDS-local, so `apply_outcome`
        // returns no SSH context and there is nothing to supervise.
        #[cfg(not(feature = "remote"))]
        {
            let _ = ssh_ctx;
            self.core.pending_srv_tx = None;
        }
        self.core.reflect_bootstrap_outcome();
        // The local link is up; if the user asked for a remote server,
        // ask the daemon to federate it now (issue #121). Idempotent, so
        // this also re-federates after a reconnect.
        self.core.federate_desired_peer();
        self.on_link_restored(now);
    }

    /// A bootstrap failed for good — a first connect or a manual reconnect
    /// that failed, or a refusal that ends automatic reconnect: surface it
    /// behind the disconnected banner.
    fn on_bootstrap_failure(&mut self, reason: String, now: Instant) {
        self.core.pending_srv_tx = None;
        self.end_outage(now, false);
        // Stash so it survives teardown and is re-printed to stderr; the
        // disconnect overlay shows the same text in-window.
        self.core.last_exit_error = Some(reason.clone());
        self.core
            .enter_disconnected(DisconnectReason::BootstrapFailed(reason));
    }

    /// The user cancelled a bootstrap they started.
    fn on_bootstrap_cancelled(&mut self) {
        self.core.pending_srv_tx = None;
        if matches!(self.core.mode, Mode::Connecting { .. }) {
            self.core
                .enter_disconnected(DisconnectReason::BootstrapFailed("cancelled".to_string()));
        }
    }

    /// The link is up again at `now`: stop retrying and deliver what was
    /// typed while it was down (issue #208) — if the link reached the same
    /// daemon run; to another run's shells it is dropped instead. The panes
    /// were already re-attached by `apply_outcome`.
    fn on_link_restored(&mut self, now: Instant) {
        let same_daemon = self.core.mgr.link_reached_same_daemon();
        self.end_outage(now, same_daemon);
    }

    /// Stop retrying and settle the input held meanwhile: sent in order when
    /// `deliver`, otherwise dropped (and counted).
    fn end_outage(&mut self, now: Instant, deliver: bool) {
        self.reconnect.on_connected();
        for msg in self.outage.flush(now, deliver) {
            self.core.mgr.send_prepared(msg);
        }
    }

    /// Apply transport upgrades and react to a dead SSH tunnel: whether either
    /// changed what is shown.
    fn drain_remote(&mut self, now: Instant) -> bool {
        // One body for both builds, so the mutation job (default features)
        // mutates the code it compiles: a lean (UDS-only) build has no
        // transport supervisor and no tunnel, and so nothing to drain.
        #[cfg(feature = "remote")]
        let changed = [
            self.drain_transport_upgrades(),
            self.drain_tunnel_deaths(now),
        ];
        #[cfg(not(feature = "remote"))]
        let changed = {
            let _ = (&*self, now);
            [false]
        };
        changed.contains(&true)
    }

    /// Apply any better-transport signals from the background probe.
    #[cfg(feature = "remote")]
    fn drain_transport_upgrades(&mut self) -> bool {
        let mut dirty = false;
        while let Ok(signal) = self.upgrade_rx.try_recv() {
            let _ = signal.sender.send(ClientMessage::ChannelReady);
            self.core
                .mgr
                .apply_transport_upgrade(signal.sender, signal.new_kind);
            dirty = true;
        }
        dirty
    }

    /// Freeze the session if the SSH tunnel process exited while we are on the
    /// tunnelled transport.
    #[cfg(feature = "remote")]
    fn drain_tunnel_deaths(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        while self.tunnel_died_rx.try_recv().is_ok() {
            if self.core.mgr.current_transport == TransportKind::TcpTls
                && self.core.mgr.connection_state().is_live()
            {
                self.link_lost(&DisconnectReason::SshTunnelDied, now);
                dirty = true;
            }
        }
        dirty
    }

    /// Send a liveness ping and detect a timeout (evaluated at [`LIVENESS_TICK`]).
    fn tick_liveness(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_liveness) < LIVENESS_TICK {
            return false;
        }
        self.last_liveness = now;
        self.core.mgr.maybe_send_client_ping(now);
        if self.core.mgr.is_liveness_timed_out(now) && self.core.mgr.connection_state().is_live() {
            self.link_lost(&DisconnectReason::PingTimeout, now);
            return true;
        }
        false
    }

    /// While the process overview is open (issue #122), re-request a snapshot at
    /// [`PROCESS_OVERVIEW_TICK`]. Returns whether a request was sent (so the view
    /// repaints on the eventual reply, not here — the reply arrives async). Does
    /// nothing in any other mode, so an idle daemon is never polled.
    fn tick_process_overview(&mut self, now: Instant) -> bool {
        if !matches!(self.core.mode, Mode::ProcessOverview) {
            return false;
        }
        if now.duration_since(self.last_process_overview) < PROCESS_OVERVIEW_TICK {
            return false;
        }
        self.last_process_overview = now;
        self.core.mgr.request_process_overview();
        false
    }

    /// Re-request the active session's client list at [`CONNECTED_CLIENTS_TICK`]
    /// while the connected-clients view is open (issue #146). Returns whether a
    /// request was sent (the view repaints on the async reply). No-op in any other
    /// mode, so an idle daemon is never polled.
    fn tick_connected_clients(&mut self, now: Instant) -> bool {
        if !matches!(self.core.mode, Mode::ConnectedClients) {
            return false;
        }
        if now.duration_since(self.last_connected_clients) < CONNECTED_CLIENTS_TICK {
            return false;
        }
        self.last_connected_clients = now;
        if let Some(word) = self.core.mgr.active_session.clone() {
            self.core.mgr.request_client_list(word);
        }
        false
    }

    /// Flush one metrics sample at [`METRICS_FLUSH_TICK`]. Never forces a redraw.
    fn tick_metrics(&mut self, now: Instant) {
        if now.duration_since(self.last_metrics_flush) >= METRICS_FLUSH_TICK {
            self.last_metrics_flush = now;
            let conn_id = self.core.mgr.connection_id;
            self.core.mgr.metrics.flush_sample(conn_id);
        }
    }

    /// Advance the cursor-blink phase. Returns whether the visible state changed.
    fn tick_blink(&mut self, now: Instant) -> bool {
        let cursor_blinks = self.core.cursor_blink_enabled
            && self.core.mgr.active_grid().is_some_and(|g| {
                let c = g.cursor();
                // Shape == Hidden ⇒ !visible, so `visible && blink` excludes hidden.
                c.visible && c.blink
            });
        let (blink_on, blink_start, changed) =
            advance_blink(self.blink_on, self.blink_phase_start, cursor_blinks, now);
        self.blink_on = blink_on;
        self.blink_phase_start = blink_start;
        changed
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    /// Dispatch a toolkit-agnostic [`Action`]. Reconnect / server-switch results
    /// are applied internally (channel rebuild + bootstrap); clipboard / quit
    /// results are returned as [`FrontendEffect`]s.
    pub fn dispatch_action(&mut self, action: Action) -> Vec<FrontendEffect> {
        let mut effects = Vec::new();
        let result = self.core.dispatch_action(action);
        self.handle_key_result(result, &mut effects);
        effects
    }

    /// Apply a pointer-driven top-bar action (server badge / session picker /
    /// pane tab click). Same effect handling as [`Self::dispatch_action`].
    pub fn apply_top_bar_action(
        &mut self,
        action: crate::core::TopBarAction,
    ) -> Vec<FrontendEffect> {
        let mut effects = Vec::new();
        if let Some(result) = self.core.apply_top_bar_action(action) {
            self.handle_key_result(result, &mut effects);
        }
        effects
    }

    /// Activate the current picker's selection (a click on a list item). Same
    /// effect handling as [`Self::dispatch_action`].
    pub fn activate_picker_selection(&mut self) -> Vec<FrontendEffect> {
        let mut effects = Vec::new();
        if let Some(result) = self.core.activate_picker_selection() {
            self.handle_key_result(result, &mut effects);
        }
        effects
    }

    fn handle_key_result(&mut self, result: KeyResult, effects: &mut Vec<FrontendEffect>) {
        match result {
            KeyResult::Continue => {}
            KeyResult::Quit => effects.push(FrontendEffect::Quit),
            KeyResult::Reconnect => self.reconnect(),
            KeyResult::ResetRenderer => effects.push(FrontendEffect::ResetRenderer),
            KeyResult::CopyToClipboard(text) => effects.push(FrontendEffect::CopyToClipboard(
                sanitize_clipboard_text(&text).into_owned(),
            )),
            KeyResult::RequestPaste => effects.push(FrontendEffect::RequestPaste),
            KeyResult::Attention {
                word_id,
                pane_id,
                kind,
                title,
                body,
                attention_id,
            } => effects.push(FrontendEffect::Attention {
                word_id,
                pane_id,
                kind,
                title,
                body,
                attention_id,
            }),
        }
    }

    /// "Reconnect now". While the link is being retried on its own, the next
    /// attempt starts at once; otherwise the server + bootstrap channels are
    /// rebuilt and a fresh bootstrap to the current target starts, behind the
    /// connecting overlay. The SSH supervisor (if any) is launched from the
    /// next [`tick`](Self::tick) when the bootstrap completes.
    pub fn reconnect(&mut self) {
        self.reconnect_at(Instant::now());
    }

    /// [`Self::reconnect`] at `now`.
    fn reconnect_at(&mut self, now: Instant) {
        if self.reconnect.is_active() {
            self.reconnect.retry_now(now);
        } else {
            self.start_bootstrap_with(BootstrapPhase::Reconnect);
        }
        self.core.request_render();
    }

    /// The banner a frontend shows about the link: reconnecting (with the
    /// attempt, the wait and the input held meanwhile), unreachable, or the
    /// keystrokes an outage dropped. `None` while there is nothing to say.
    pub fn connection_banner(&self) -> Option<ConnectionBanner> {
        connection_banner(&self.reconnect, &self.outage, Instant::now())
    }

    /// Forward a batch of key events to the active pane's PTY, and reset the
    /// blink cycle so typing shows a solid cursor. While the link is down they
    /// are held for delivery on reconnect.
    pub fn send_keys(&mut self, keys: Vec<KeyEvent>) {
        self.resume_if_auto_paused();
        let built = self.core.mgr.key_batch_message(keys);
        self.deliver_input(built);
        self.blink_on = true;
        self.blink_phase_start = Instant::now();
    }

    /// Send user input now, or — while the link is down and being retried —
    /// hold it for delivery on reconnect (issue #208).
    fn deliver_input(&mut self, built: Result<ClientMessage, bool>) {
        let Ok(msg) = built else {
            return;
        };
        if self.reconnect.is_active() {
            self.outage.push(msg, Instant::now());
            self.core.request_render();
        } else {
            self.core.mgr.send_prepared(msg);
        }
    }

    /// Forward raw bytes to the active pane's PTY (e.g. mouse-report sequences).
    pub fn send_input(&mut self, bytes: Vec<u8>) {
        self.core.mgr.send_input(bytes);
    }

    /// Feed clipboard text back as a paste (in response to
    /// [`FrontendEffect::RequestPaste`]).
    pub fn feed_paste(&mut self, text: String) {
        self.resume_if_auto_paused();
        let built = self.core.mgr.paste_message(text);
        self.deliver_input(built);
    }

    /// Resume an *auto*-paused connection so the user immediately sees the output
    /// of what they type (issue #165). A keypress means the user is back, so the
    /// stream should catch up — reconciliation is minimal (the re-attach replies
    /// with one final snapshot, not a frame-by-frame replay). A *manual* pause is
    /// deliberate and left alone: its input is dropped downstream
    /// (`SessionManager::input_suppressed`) until the user toggles it off.
    ///
    /// Resume runs *before* the input is forwarded, so on the wire the daemon
    /// sees `SetPaused(false)` → `Attach` → the keystroke, and the echo streams
    /// back over the now-resumed connection. `set_auto_pause` is idempotent, so
    /// only the first keystroke of a burst does any work.
    fn resume_if_auto_paused(&mut self) {
        if self.core.auto_pause && !self.core.manual_pause {
            self.core.set_auto_pause(false);
            // Disarm the background debounce so a still-armed timer can't
            // re-pause the connection the user just resumed by typing.
            self.background_since = None;
        }
    }

    /// Report a new content size immediately (no debounce). Used to seed the
    /// initial size before the first connect.
    pub fn set_term_size(&mut self, size: TermSize) {
        self.core.set_term_size(size);
    }

    /// Report a new content size, debounced: the size is applied from a later
    /// [`tick`](Self::tick) once the resize burst settles.
    pub fn request_resize(&mut self, size: TermSize) {
        self.pending_resize = Some(size);
        self.resize_deadline = Some(Instant::now() + RESIZE_DEBOUNCE);
    }

    /// Report whether the app window is backgrounded/minimized/occluded, for
    /// auto-pause (issue #68). Backgrounding arms a debounce (the connection
    /// auto-pauses from a later [`tick`](Self::tick) if still backgrounded after
    /// `AUTO_PAUSE_DEBOUNCE`); foregrounding resumes immediately. A *manual*
    /// pause is unaffected and persists across focus changes.
    pub fn set_window_background(&mut self, backgrounded: bool) {
        if backgrounded {
            // Local-daemon connections never auto-pause (issue #165), so don't
            // bother arming the debounce for them — `set_auto_pause` would no-op
            // anyway, this just avoids the idle per-frame `tick_auto_pause` check.
            if self.background_since.is_none() && !self.core.auto_pause && !self.core.is_local {
                self.background_since = Some(Instant::now());
            }
        } else {
            self.background_since = None;
            self.core.set_auto_pause(false);
        }
    }

    /// Apply the armed auto-pause once the debounce elapses. Returns whether the
    /// pause state changed (so the caller flags a render for the indicator).
    fn tick_auto_pause(&mut self, now: Instant) -> bool {
        if let Some(since) = self.background_since
            && !self.core.auto_pause
            && now.duration_since(since) >= AUTO_PAUSE_DEBOUNCE
        {
            self.core.set_auto_pause(true);
            return true;
        }
        false
    }

    /// Snap the active pane's viewport back to the live bottom (e.g. on keypress).
    pub fn scroll_to_bottom(&mut self) {
        if let Some(grid) = self.core.mgr.active_grid_mut() {
            grid.scroll_to_bottom();
        }
    }

    // ── State out ───────────────────────────────────────────────────────────

    /// The active pane's grid to paint, if any.
    pub fn active_grid(&self) -> Option<&CellGrid> {
        self.core.mgr.active_grid()
    }

    /// Whether the cursor is shown on the current frame (blink phase).
    pub fn blink_on(&self) -> bool {
        self.blink_on
    }

    /// Whether the render-debug overlay is shown (the frontend reconciles its
    /// overlay against this each pump).
    pub fn render_debug_visible(&self) -> bool {
        self.core.render_debug_visible
    }

    /// Assemble a [`crate::core::RenderDebugSnapshot`] for the focused pane, supplying the
    /// driver's current blink phase. The frontend passes its own pixel/scale/
    /// renderer context.
    pub fn render_debug_snapshot(
        &self,
        frame_width: u32,
        frame_height: u32,
        scale: f32,
        renderer: &str,
    ) -> crate::core::RenderDebugSnapshot {
        self.core
            .render_debug_snapshot(frame_width, frame_height, scale, renderer, self.blink_on)
    }

    /// Borrow the wrapped [`AppCore`] (read). Most frontends reach core state
    /// through [`Deref`] instead; this is the explicit handle for an FFI layer.
    pub fn core(&self) -> &AppCore {
        &self.core
    }

    /// Mutate the core, and ask for a frame.
    ///
    /// Every frontend mutation is followed by a render request, and the two used
    /// to be separate statements at fifty-odd call sites across `kmux-ffi` and
    /// `kmux-gtk`. Forgetting the second is silent: the state changes and the
    /// user sees the previous frame until something unrelated repaints. Here the
    /// request is not something a caller remembers.
    ///
    /// This replaces `core_mut()` and `DerefMut`, which handed every frontend a
    /// `&mut` to all of `AppCore` for the sake of one field.
    pub fn mutate<T>(&mut self, f: impl FnOnce(&mut AppCore) -> T) -> T {
        let out = f(&mut self.core);
        self.core.request_render();
        out
    }

    /// Borrow the session manager mutably.
    ///
    /// The one piece of core state a frontend legitimately drives: selection,
    /// scrolling, pane sizes and mouse reporting all live on it and all follow
    /// the pointer, at a rate the driver's tick already repaints for. Naming it
    /// is the point — with `DerefMut` gone, this is the only field of `AppCore`
    /// a frontend can still reach into, instead of all forty-eight.
    pub fn mgr_mut(&mut self) -> &mut kmux_client::session_manager::SessionManager {
        &mut self.core.mgr
    }

    /// Borrow the core mutably *without* requesting a frame.
    ///
    /// For the one caller that mutates only to put the state back: building
    /// command-palette hints needs a `Mode::Command` to read the buffer out of,
    /// and restores the previous mode before returning. Nothing changed, so
    /// nothing needs repainting.
    pub fn core_for_query(&mut self) -> &mut AppCore {
        &mut self.core
    }

    /// Ask the frontend for a frame. See [`AppCore::request_render`].
    pub fn request_render(&mut self) {
        self.core.request_render();
    }
}

/// Frontends read core state directly (`driver.mgr`, `driver.mode`,
/// `driver.palette`, …) through this deref.
impl Deref for FrontendDriver {
    type Target = AppCore;
    fn deref(&self) -> &AppCore {
        &self.core
    }
}

#[cfg(test)]
impl FrontendDriver {
    /// Build a driver around `core` *without* starting a bootstrap, returning the
    /// server-message and bootstrap-outcome senders so a test can inject events.
    fn for_test(
        core: AppCore,
    ) -> (
        Self,
        mpsc::UnboundedSender<ServerMessage>,
        mpsc::UnboundedSender<BootstrapTaskResult>,
    ) {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (bs_tx, bs_rx) = mpsc::unbounded_channel::<BootstrapTaskResult>();
        #[cfg(feature = "remote")]
        let (upgrade_tx, upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
        #[cfg(feature = "remote")]
        let (tunnel_died_tx, tunnel_died_rx) = mpsc::channel::<()>(1);
        let now = Instant::now();
        let last_palette = core.palette.clone();
        let driver = Self {
            core,
            srv_rx,
            bootstrap_rx: Some(bs_rx),
            #[cfg(feature = "remote")]
            upgrade_rx,
            #[cfg(feature = "remote")]
            upgrade_tx,
            #[cfg(feature = "remote")]
            tunnel_died_rx,
            #[cfg(feature = "remote")]
            tunnel_died_tx,
            last_palette,
            last_liveness: now,
            last_metrics_flush: now,
            last_process_overview: now,
            last_connected_clients: now,
            pending_resize: None,
            resize_deadline: None,
            blink_on: true,
            blink_phase_start: now,
            tear_state: HashMap::new(),
            tear_window_ms: TEAR_WINDOW_MS,
            tear_min_ops: TEAR_MIN_OPS,
            background_since: None,
            tick_id: 0,
            trace: None,
            // Seed 0: no jitter, so a test can name every delay.
            reconnect: Reconnect::new(0),
            outage: OutageInput::default(),
            last_banner: None,
        };
        (driver, srv_tx, bs_tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::ClientCapabilities;

    fn fixture_core() -> AppCore {
        let mgr = SessionManager::new(
            "127.0.0.1".into(),
            0,
            String::new(),
            true,
            ClientCapabilities::default(),
        );
        AppCore::for_test(mgr)
    }

    // ── Automatic reconnect (issue #208) ─────────────────────────────────────

    use kmux_client::connection_state::ConnectionState;

    /// A driver whose link is live, typing into `eagle/0`, and the receiving
    /// end of that link.
    fn live_driver() -> (
        FrontendDriver,
        mpsc::UnboundedSender<ServerMessage>,
        mpsc::UnboundedSender<BootstrapTaskResult>,
        mpsc::UnboundedReceiver<ClientMessage>,
    ) {
        let (mut driver, srv_tx, bs_tx) = FrontendDriver::for_test(fixture_core());
        let (tx, rx) = mpsc::unbounded_channel();
        driver.core.mgr.apply_outcome(link(tx, Some(DAEMON_RUN)));
        driver.core.mgr.active_pane = Some("eagle/0".to_string());
        (driver, srv_tx, bs_tx, rx)
    }

    use kmux_protocol::messages::DaemonInstanceId;

    /// The daemon run `live_driver` is linked to.
    const DAEMON_RUN: DaemonInstanceId = DaemonInstanceId(4242);

    /// A key typing `c`.
    fn typed(c: char) -> KeyEvent {
        use kmux_protocol::messages::{KeyAction, KeyCode, KeyMods};
        KeyEvent {
            code: KeyCode::A,
            mods: KeyMods::default(),
            action: KeyAction::Press,
            text: c.to_string(),
            unshifted_codepoint: 0,
        }
    }

    /// The text of every key batch sent on `rx`, in order.
    fn sent_text(rx: &mut mpsc::UnboundedReceiver<ClientMessage>) -> String {
        let mut text = String::new();
        while let Ok(msg) = rx.try_recv() {
            if let ClientMessage::PtyKeyBatch { events, .. } = msg {
                text.extend(events.iter().map(|e| e.text.as_str()));
            }
        }
        text
    }

    /// A successful local bootstrap whose link sends into `tx`.
    fn outcome(tx: mpsc::UnboundedSender<ClientMessage>) -> BootstrapTaskResult {
        BootstrapTaskResult::Success(Box::new(link(tx, Some(DAEMON_RUN))))
    }

    /// A local link sending into `tx`, to daemon run `run`.
    fn link(
        tx: mpsc::UnboundedSender<ClientMessage>,
        run: Option<DaemonInstanceId>,
    ) -> kmux_client::pipeline::BootstrapOutcome {
        use kmux_client::pipeline::BootstrapOutcome;
        use kmux_client::transport::TransportKind as Link;
        use kmux_protocol::messages::ConnectionId;
        BootstrapOutcome {
            client_tx: tx,
            transport: Link::Uds,
            host: String::new(),
            port: 0,
            token: String::new(),
            capabilities: ClientCapabilities::default(),
            accept_invalid_certs: false,
            connection_id: ConnectionId(1),
            server_version: None,
            is_local: true,
            ssh_context: None,
            bootstrap_elapsed: Duration::ZERO,
            daemon_instance: run,
        }
    }

    /// A dropped link no longer freezes the UI behind a prompt: the mode is
    /// unchanged, the badge reads `Reconnecting`, the banner offers
    /// "Reconnect now", and the first attempt is scheduled.
    #[test]
    fn a_dropped_link_keeps_the_ui_and_schedules_a_reconnect() {
        let (mut driver, srv_tx, _bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        let effects = driver.tick_at(t0);

        assert!(matches!(driver.mode, Mode::Normal));
        assert_eq!(
            driver.mgr.connection_state(),
            &ConnectionState::Reconnecting { attempt: 1 }
        );
        assert!(effects.contains(&FrontendEffect::NeedsRender));
        assert_eq!(
            driver.reconnect.next_in(t0),
            Some(kmux_client::backoff::next_delay(0, 0))
        );
        let banner = driver.connection_banner().expect("a banner");
        assert!(banner.reconnecting);
    }

    /// Keys typed during the outage are held, not sent into a dead link, and
    /// reach the new link in order once it is up; the retries stop.
    #[test]
    fn keys_typed_during_an_outage_are_delivered_in_order_on_reconnect() {
        let (mut driver, srv_tx, bs_tx, mut old_rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.send_keys(vec![typed('l'), typed('s')]);
        driver.feed_paste(" -la".to_string());
        driver.send_keys(vec![typed('\r')]);
        assert_eq!(sent_text(&mut old_rx), "", "nothing goes to the dead link");
        assert_eq!(driver.outage.queued(), 4);

        // An attempt under way succeeds.
        driver.reconnect.retry_now(t0);
        assert_eq!(driver.reconnect.take_due(t0), Some(1));
        let (tx, mut new_rx) = mpsc::unbounded_channel();
        bs_tx.send(outcome(tx)).unwrap();
        driver.tick_at(t0);

        let mut pasted = String::new();
        let mut keys = String::new();
        while let Ok(msg) = new_rx.try_recv() {
            match msg {
                ClientMessage::PtyKeyBatch { events, .. } => {
                    keys.extend(events.iter().map(|e| e.text.as_str()));
                    pasted.push('|');
                }
                ClientMessage::PtyPaste { data, .. } => pasted.push_str(&data),
                _ => {}
            }
        }
        assert_eq!(keys, "ls\r");
        assert_eq!(pasted, "| -la|", "the paste lands between the two batches");
        assert!(!driver.reconnect.is_active(), "the retries stop");
        assert_eq!(driver.outage.queued(), 0);

        // And typing goes straight out again.
        driver.send_keys(vec![typed('x')]);
        assert_eq!(sent_text(&mut new_rx), "x");
    }

    /// A failed attempt schedules the next with the backoff, keeps the UI as
    /// it is, and says why in the banner.
    #[test]
    fn a_failed_reconnect_attempt_schedules_the_next() {
        let (mut driver, srv_tx, bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.reconnect.retry_now(t0);
        driver.reconnect.take_due(t0);

        bs_tx
            .send(BootstrapTaskResult::Failed("refused".to_string()))
            .unwrap();
        driver.tick_at(t0);

        assert!(matches!(driver.mode, Mode::Normal));
        assert_eq!(driver.reconnect.attempt(), Some(2));
        assert_eq!(
            driver.reconnect.next_in(t0),
            Some(kmux_client::backoff::next_delay(1, 0))
        );
        let banner = driver.connection_banner().expect("a banner");
        assert!(banner.text.ends_with("— refused"), "{}", banner.text);
    }

    /// A dropped link whose attempt under way is refused (the protocol range
    /// or the token) stops retrying: the banner is replaced by the
    /// disconnected mode, and input held meanwhile is dropped and counted.
    #[test]
    fn a_refused_reconnect_attempt_stops_retrying() {
        let (mut driver, srv_tx, bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.send_keys(vec![typed('l')]);
        driver.reconnect.retry_now(t0);
        driver.reconnect.take_due(t0);

        bs_tx
            .send(BootstrapTaskResult::Refused("auth rejected".to_string()))
            .unwrap();
        driver.tick_at(t0);

        assert!(!driver.reconnect.is_active());
        assert!(matches!(driver.mode, Mode::Disconnected { .. }));
        assert_eq!(driver.outage.dropped(), 1);
        assert_eq!(driver.last_exit_error.as_deref(), Some("auth rejected"));
    }

    /// An automatic attempt whose task ends with no result counts as a
    /// failed attempt; a cancelled manual bootstrap is a disconnect.
    #[test]
    fn a_bootstrap_ending_without_a_result_is_a_failed_attempt_or_a_cancel() {
        let (mut driver, srv_tx, bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.reconnect.retry_now(t0);
        driver.reconnect.take_due(t0);
        drop(bs_tx);
        driver.tick_at(t0);
        assert_eq!(driver.reconnect.attempt(), Some(2));
        assert_eq!(
            driver.reconnect.last_error(),
            Some("reconnect attempt cancelled")
        );

        let (mut driver, _srv_tx, bs_tx) = FrontendDriver::for_test(fixture_core());
        driver.mutate(|core| {
            core.mode = Mode::Connecting {
                target_display: "x".into(),
            }
        });
        drop(bs_tx);
        driver.tick_at(Instant::now());
        assert!(matches!(driver.mode, Mode::Disconnected { .. }));
        assert!(!driver.reconnect.is_active());
    }

    /// A live link that goes silent past the liveness timeout is lost and
    /// retried; one that is merely quiet for a moment is not.
    #[test]
    fn a_silent_link_is_lost_at_the_liveness_timeout() {
        let (mut driver, _srv_tx, _bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        driver.tick_at(t0 + Duration::from_secs(2));
        assert!(driver.mgr.connection_state().is_live());

        driver.tick_at(t0 + kmux_client::liveness::TIMEOUT + Duration::from_secs(2));
        assert_eq!(
            driver.mgr.connection_state(),
            &ConnectionState::Reconnecting { attempt: 1 }
        );
    }

    /// The SSH tunnel dying under a tunnelled link loses it; under another
    /// transport it changes nothing.
    #[cfg(feature = "remote")]
    #[test]
    fn a_dead_ssh_tunnel_loses_only_a_tunnelled_link() {
        let (mut driver, _srv_tx, _bs_tx, _rx) = live_driver();
        driver.tunnel_died_tx.try_send(()).unwrap();
        driver.tick_at(Instant::now());
        assert!(driver.mgr.connection_state().is_live(), "a UDS link");

        driver.core.mgr.current_transport = TransportKind::TcpTls;
        driver.tunnel_died_tx.try_send(()).unwrap();
        driver.tick_at(Instant::now());
        assert!(driver.reconnect.is_active());
    }

    /// The frame is repainted when the banner changes — here, when the
    /// notice of dropped keystrokes expires — and not while it stays put.
    #[test]
    fn the_banner_changing_repaints_the_frame() {
        let (mut driver, srv_tx, bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.send_keys(vec![typed('x'); OUTAGE_INPUT_CAPACITY + 1]);
        driver.reconnect.retry_now(t0);
        driver.reconnect.take_due(t0);
        // What starting the attempt would have done: a fresh server channel.
        let (_srv_tx, srv_rx) = mpsc::unbounded_channel();
        driver.srv_rx = srv_rx;
        let (tx, _new_rx) = mpsc::unbounded_channel();
        bs_tx.send(outcome(tx)).unwrap();
        assert!(driver.tick_at(t0).contains(&FrontendEffect::NeedsRender));
        assert!(driver.connection_banner().is_some(), "the dropped notice");

        assert!(
            !driver
                .tick_at(t0 + Duration::from_millis(1))
                .contains(&FrontendEffect::NeedsRender),
            "nothing changed"
        );
        assert!(
            driver
                .tick_at(t0 + DROPPED_NOTICE)
                .contains(&FrontendEffect::NeedsRender),
            "the notice expired"
        );
    }

    /// The banner reports a change once, then nothing until it changes again.
    #[test]
    fn refresh_banner_reports_each_change_once() {
        let (mut driver, _srv_tx, _bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        assert!(!driver.refresh_banner(t0), "no banner, no change");
        driver.reconnect.on_lost(t0);
        assert!(driver.refresh_banner(t0), "a banner appeared");
        assert!(!driver.refresh_banner(t0), "the same banner");
        assert!(
            driver.refresh_banner(t0 + Duration::from_secs(1)),
            "the countdown moved on"
        );
    }

    /// "Reconnect now" while retrying brings the next attempt forward
    /// instead of starting a second bootstrap.
    #[test]
    fn reconnect_now_while_retrying_brings_the_attempt_forward() {
        let (mut driver, srv_tx, _bs_tx, _rx) = live_driver();
        drop(srv_tx);
        driver.tick_at(Instant::now());
        driver.reconnect();
        assert_eq!(
            driver.reconnect.next_in(Instant::now()),
            Some(Duration::ZERO)
        );
        assert!(
            !matches!(driver.mode, Mode::Connecting { .. }),
            "no connecting overlay"
        );
    }

    // The two tests below start a bootstrap, which spawns its task. On the
    // current-thread test runtime, with no `.await` after it, that task is
    // never polled — nothing dials a daemon — and is dropped with the runtime.

    /// A due attempt starts on the tick: the badge reads its number and a
    /// bootstrap is in flight, with no connecting overlay.
    #[tokio::test]
    async fn a_due_reconnect_attempt_starts_on_the_tick() {
        let (mut driver, srv_tx, _bs_tx, _rx) = live_driver();
        let t0 = Instant::now();
        drop(srv_tx);
        driver.tick_at(t0);
        driver.core.cancel_tx = None;

        let due = t0 + kmux_client::backoff::next_delay(0, 0);
        let effects = driver.tick_at(due);

        assert!(effects.contains(&FrontendEffect::NeedsRender));
        assert_eq!(driver.reconnect.next_in(due), None, "under way");
        assert!(driver.core.cancel_tx.is_some(), "a bootstrap is in flight");
        assert_eq!(
            driver.mgr.connection_state(),
            &ConnectionState::Reconnecting { attempt: 1 }
        );
        assert!(matches!(driver.mode, Mode::Normal));
    }

    /// "Reconnect" with no outage under way is the manual reconnect, behind
    /// the connecting overlay.
    #[tokio::test]
    async fn reconnect_with_no_outage_starts_a_manual_reconnect() {
        let (mut driver, _srv_tx, _bs_tx, _rx) = live_driver();
        driver.reconnect();
        assert!(matches!(driver.mode, Mode::Connecting { .. }));
        assert!(!driver.reconnect.is_active());
    }

    /// A link that was never up (the first bootstrap still pending) is not
    /// "lost": a closed channel then schedules nothing.
    #[test]
    fn a_closed_channel_before_the_first_connect_schedules_no_reconnect() {
        let (mut driver, srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        drop(srv_tx);
        driver.tick_at(Instant::now());
        assert!(!driver.reconnect.is_active());
    }

    #[test]
    fn palette_change_emits_palette_changed() {
        // Mutating the live palette (as the `/theme` command does) is detected on
        // the next tick and reported so the frontend reloads chrome styling.
        let (mut driver, _srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        let other = crate::theme::builtin_theme("dracula").unwrap();
        assert_ne!(&driver.palette, &other, "fixture must differ from dracula");
        driver.mutate(|core| core.palette = other);
        let effects = driver.tick();
        assert!(effects.contains(&FrontendEffect::PaletteChanged));
        assert!(effects.contains(&FrontendEffect::NeedsRender));
    }

    #[test]
    fn no_palette_change_does_not_emit_palette_changed() {
        // An unchanged palette must not spuriously trigger a chrome reload.
        let (mut driver, _srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        let effects = driver.tick();
        assert!(!effects.contains(&FrontendEffect::PaletteChanged));
    }

    #[test]
    fn failed_bootstrap_stashes_error_and_disconnects() {
        // A failed bootstrap outcome stashes the error (re-printed on exit) and
        // shows the disconnect overlay.
        let (mut driver, _srv_tx, bs_tx) = FrontendDriver::for_test(fixture_core());
        bs_tx
            .send(BootstrapTaskResult::Failed("boom".to_string()))
            .unwrap();
        let _ = driver.tick();
        assert_eq!(driver.last_exit_error.as_deref(), Some("boom"));
        assert!(matches!(driver.mode, Mode::Disconnected { .. }));
    }

    // ── Keyboard-triggered resume (issue #165) ───────────────────────────────

    /// A keystroke resumes an *auto*-paused connection so the user immediately
    /// sees their own output, and disarms the background debounce so a still-armed
    /// timer can't re-pause it. A *manual* pause is deliberate and left untouched.
    #[test]
    fn keystroke_resumes_auto_pause_but_not_manual_pause() {
        let mut core = fixture_core();
        core.is_local = false; // remote server: auto-pause is in play
        let (mut driver, _srv_tx, _bs_tx) = FrontendDriver::for_test(core);

        // Auto-paused with the background debounce still armed.
        driver.core.set_auto_pause(true);
        driver.background_since = Some(Instant::now());
        assert!(driver.core.auto_pause);

        // Typing resumes the connection and disarms the debounce.
        driver.resume_if_auto_paused();
        assert!(
            !driver.core.auto_pause,
            "a keystroke must resume an auto-pause"
        );
        assert!(
            driver.background_since.is_none(),
            "the debounce must be disarmed"
        );

        // A manual pause must survive a keystroke (its input is dropped instead).
        driver.core.toggle_manual_pause();
        assert!(driver.core.manual_pause);
        driver.resume_if_auto_paused();
        assert!(
            driver.core.manual_pause,
            "a manual pause must not be resumed by typing"
        );
    }

    // ── Tearing detector (issue #72) ─────────────────────────────────────────

    #[test]
    fn tear_detected_logic() {
        // No prior paint → never a tear.
        assert!(!tear_detected(None, 1_000, 16));
        // Within the window → the previous paint showed a partial frame.
        assert!(tear_detected(Some(1_000), 1_008, 16));
        // Exactly the window → not within (strict `<`).
        assert!(!tear_detected(Some(1_000), 1_016, 16));
        // Beyond the window → two distinct logical frames, not a tear.
        assert!(!tear_detected(Some(1_000), 1_050, 16));
        // This tick's diff predates the painted one (reorder) → not a forward tear.
        assert!(!tear_detected(Some(1_000), 990, 16));
    }

    fn cell_update(pane: &str, seqno: u64, sent_at_ms: u64, ops: usize) -> ServerMessage {
        use kmux_protocol::messages::{CellState, CursorState, DiffOp, SequenceNo, TerminalDiff};
        use std::sync::Arc;
        let ops_vec = (0..ops)
            .map(|i| DiffOp::Cell {
                row: 0,
                col: i as u16,
                cell: CellState::default(),
            })
            .collect();
        ServerMessage::TerminalUpdate {
            pane_id: pane.to_string(),
            diff: Arc::new(TerminalDiff {
                ops: ops_vec,
                cursor: CursorState::default(),
                modes: kmux_protocol::messages::TermModes::EMPTY,
                history_total: 0,
                scrollback_reset: None,
            }),
            seqno: SequenceNo(seqno),
            sent_at_ms,
        }
    }

    #[test]
    fn split_logical_frame_across_ticks_counts_a_tear() {
        // Two cell diffs emitted 8ms apart (one logical frame) but delivered in
        // separate pump ticks → the first paint was partial → one tear.
        let (mut driver, srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        srv_tx.send(cell_update("pane", 0, 1_000, 8)).unwrap();
        let _ = driver.tick();
        srv_tx.send(cell_update("pane", 1, 1_008, 8)).unwrap();
        let _ = driver.tick();
        assert_eq!(driver.core.mgr.metrics.snapshot(false).counters.tears, 1);
    }

    #[test]
    fn frame_painted_atomically_is_not_a_tear() {
        // Both halves of the logical frame arrive in the SAME tick → painted
        // together → no tear. A later, well-separated frame is also clean.
        let (mut driver, srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        srv_tx.send(cell_update("pane", 0, 1_000, 8)).unwrap();
        srv_tx.send(cell_update("pane", 1, 1_004, 8)).unwrap();
        let _ = driver.tick();
        srv_tx.send(cell_update("pane", 2, 2_000, 8)).unwrap();
        let _ = driver.tick();
        assert_eq!(driver.core.mgr.metrics.snapshot(false).counters.tears, 0);
    }

    #[test]
    fn sub_min_ops_diffs_do_not_count() {
        // Tiny diffs (keystroke echoes) are below TEAR_MIN_OPS and never tear,
        // even when delivered in adjacent ticks within the window.
        let (mut driver, srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        srv_tx.send(cell_update("pane", 0, 1_000, 1)).unwrap();
        let _ = driver.tick();
        srv_tx.send(cell_update("pane", 1, 1_005, 1)).unwrap();
        let _ = driver.tick();
        assert_eq!(driver.core.mgr.metrics.snapshot(false).counters.tears, 0);
    }
}
