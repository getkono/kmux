//! [`FrontendDriver`]: the toolkit-agnostic run-loop orchestration shared by
//! every frontend.
//!
//! [`AppCore`] is a passive state machine; *driving* it has always meant the
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
//! - builds an [`AppCore`] with its own capabilities, wraps it with
//!   [`FrontendDriver::new`] (which creates the channels and kicks off the
//!   initial bootstrap),
//! - calls [`FrontendDriver::tick`] once per frame from its own loop (a glib
//!   timeout, a `CVDisplayLink`, …) and acts on the returned [`FrontendEffect`]s
//!   (repaint, copy to clipboard, request paste, quit),
//! - feeds input in via [`dispatch_action`](FrontendDriver::dispatch_action),
//!   [`send_keys`](FrontendDriver::send_keys), [`request_resize`], the picker
//!   drivers, …,
//! - reads state out via [`Deref`] to [`AppCore`] (`driver.mgr`, `driver.mode`,
//!   `driver.palette`, …) plus [`active_grid`](FrontendDriver::active_grid) and
//!   [`blink_on`](FrontendDriver::blink_on).
//!
//! It owns no run loop and no runtime: it assumes an *ambient* tokio runtime
//! (the spawning paths use the current `Handle`) exactly as the frontends do
//! today, so the caller stays in control of the loop and the runtime.
//!
//! [`request_resize`]: FrontendDriver::request_resize

mod blink;
mod clipboard;
mod frame_trace;

pub use blink::{CURSOR_BLINK_HALF, advance_blink};
pub use clipboard::sanitize_clipboard_text;

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use kmux_client::connection_state::DisconnectReason;
use kmux_client::grid::CellGrid;
#[cfg(feature = "remote")]
use kmux_client::supervisor::UpgradeSignal;
#[cfg(feature = "remote")]
use kmux_client::transport::TransportKind;
#[cfg(feature = "remote")]
use kmux_protocol::messages::ClientMessage;
use kmux_protocol::messages::{
    AttentionKind, KeyEvent, PaneId, ServerMessage, TermSize, epoch_millis,
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
    /// Implies a repaint. Read the new palette from [`FrontendDriver::palette`].
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
        let mut effects = Vec::new();
        let now = Instant::now();
        let mut dirty = false;
        self.tick_id = self.tick_id.wrapping_add(1);

        if self.detect_palette_change() {
            effects.push(FrontendEffect::PaletteChanged);
            dirty = true;
        }
        dirty |= self.apply_settled_resize(now);
        // Off-UI-thread grid apply (issue #182, §1): load any content the apply
        // worker republished since last tick, then apply the view effects /
        // resyncs it reported, before draining (and enqueueing) this tick's
        // server messages.
        dirty |= self.core.mgr.refresh_buffers();
        dirty |= self.core.mgr.drain_apply_notes();
        dirty |= self.drain_server_messages(&mut effects);
        dirty |= self.poll_bootstrap_outcome();
        #[cfg(feature = "remote")]
        {
            dirty |= self.drain_transport_upgrades();
            dirty |= self.drain_tunnel_deaths();
        }
        dirty |= self.tick_liveness(now);
        // Refresh the process overview while it is open (issue #122).
        dirty |= self.tick_process_overview(now);
        // Refresh the connected-clients list while that view is open (issue #146).
        dirty |= self.tick_connected_clients(now);
        // Auto-pause the connection once the window has been backgrounded long
        // enough (issue #68).
        dirty |= self.tick_auto_pause(now);
        // Fire any soft-close whose 3 s grace window has elapsed (issue #86).
        dirty |= self.core.fire_due_closes(now);
        self.tick_metrics(now);
        dirty |= self.tick_blink(now);

        if self.core.needs_render {
            self.core.needs_render = false;
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
    fn drain_server_messages(&mut self, effects: &mut Vec<FrontendEffect>) -> bool {
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
        if closed
            && !matches!(
                self.core.mode,
                Mode::Connecting { .. } | Mode::Disconnected { .. }
            )
        {
            self.core.enter_disconnected(DisconnectReason::ServerClosed);
            dirty = true;
        }
        dirty
    }

    /// Extract per-diff timing from a drained batch for the tearing detector and
    /// frame trace (issue #72). Returns `(applied, tick_cells)` where `applied`
    /// is every seqno/sent_at/ops applied this tick and `tick_cells` is, per
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
    fn poll_bootstrap_outcome(&mut self) -> bool {
        let outcome = self
            .bootstrap_rx
            .as_mut()
            .and_then(|rx| match rx.try_recv() {
                Ok(o) => Some(Ok(o)),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(())),
            });
        match outcome {
            Some(Ok(BootstrapTaskResult::Success(o))) => {
                self.core.cancel_tx = None;
                // A later success clears any stashed failure so we don't re-print
                // a stale error when the user finally quits.
                self.core.last_exit_error = None;
                let ssh_ctx = self.core.mgr.apply_outcome(*o);
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
                self.bootstrap_rx = None;
                true
            }
            Some(Ok(BootstrapTaskResult::Failed(reason))) => {
                self.core.cancel_tx = None;
                self.core.pending_srv_tx = None;
                // Stash so it survives teardown and is re-printed to stderr; the
                // disconnect overlay shows the same text in-window.
                self.core.last_exit_error = Some(reason.clone());
                self.core
                    .enter_disconnected(DisconnectReason::BootstrapFailed(reason));
                self.bootstrap_rx = None;
                true
            }
            Some(Err(())) => {
                // Channel closed with no result → the bootstrap was cancelled
                // (cancel_tx dropped via Action::CancelBootstrap).
                self.core.cancel_tx = None;
                self.core.pending_srv_tx = None;
                if matches!(self.core.mode, Mode::Connecting { .. }) {
                    self.core
                        .enter_disconnected(DisconnectReason::BootstrapFailed(
                            "cancelled".to_string(),
                        ));
                }
                self.bootstrap_rx = None;
                true
            }
            None => false,
        }
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
    fn drain_tunnel_deaths(&mut self) -> bool {
        let mut dirty = false;
        while self.tunnel_died_rx.try_recv().is_ok() {
            if self.core.mgr.current_transport == TransportKind::TcpTls
                && !matches!(self.core.mode, Mode::Disconnected { .. })
            {
                self.core
                    .enter_disconnected(DisconnectReason::SshTunnelDied);
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
        if self.core.mgr.is_liveness_timed_out(now)
            && !matches!(
                self.core.mode,
                Mode::Disconnected { .. } | Mode::Connecting { .. }
            )
        {
            self.core.enter_disconnected(DisconnectReason::PingTimeout);
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
    pub async fn dispatch_action(&mut self, action: Action) -> Vec<FrontendEffect> {
        let mut effects = Vec::new();
        let result = self.core.dispatch_action(action).await;
        self.handle_key_result(result, &mut effects);
        effects
    }

    /// Apply a pointer-driven top-bar action (server badge / session picker /
    /// pane tab click). Same effect handling as [`dispatch_action`].
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
    /// effect handling as [`dispatch_action`].
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

    /// Rebuild the server + bootstrap channels and start a fresh bootstrap to the
    /// current target. The SSH supervisor (if any) is launched from the next
    /// [`tick`](Self::tick) when the bootstrap completes.
    pub fn reconnect(&mut self) {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel();
        self.srv_rx = srv_rx;
        let (bs_tx, bs_rx) = mpsc::unbounded_channel();
        self.bootstrap_rx = Some(bs_rx);
        let target = self.core.current_target();
        self.core
            .start_bootstrap(target, srv_tx, BootstrapPhase::Reconnect, bs_tx);
        self.core.needs_render = true;
    }

    /// Forward a batch of key events to the active pane's PTY, and reset the
    /// blink cycle so typing shows a solid cursor.
    pub fn send_keys(&mut self, keys: Vec<KeyEvent>) {
        self.resume_if_auto_paused();
        self.core.mgr.send_key_batch(keys);
        self.blink_on = true;
        self.blink_phase_start = Instant::now();
    }

    /// Forward raw bytes to the active pane's PTY (e.g. mouse-report sequences).
    pub fn send_input(&mut self, bytes: Vec<u8>) {
        self.core.mgr.send_input(bytes);
    }

    /// Feed clipboard text back as a paste (in response to
    /// [`FrontendEffect::RequestPaste`]).
    pub fn feed_paste(&mut self, text: String) {
        self.resume_if_auto_paused();
        self.core.mgr.send_paste(text);
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
    /// [`AUTO_PAUSE_DEBOUNCE`]); foregrounding resumes immediately. A *manual*
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

    /// Assemble a [`RenderDebugSnapshot`] for the focused pane, supplying the
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

    /// Borrow the wrapped [`AppCore`] (mutate). See [`core`](Self::core).
    pub fn core_mut(&mut self) -> &mut AppCore {
        &mut self.core
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

impl DerefMut for FrontendDriver {
    fn deref_mut(&mut self) -> &mut AppCore {
        &mut self.core
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

    #[test]
    fn closed_server_channel_enters_disconnected() {
        // While live, dropping the server-message sender (its task ended /
        // transport closed) must surface as a disconnect on the next tick.
        let (mut driver, srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        assert!(matches!(driver.mode, Mode::Normal));
        drop(srv_tx);
        let effects = driver.tick();
        assert!(
            matches!(driver.mode, Mode::Disconnected { .. }),
            "a closed server channel must enter Disconnected"
        );
        assert!(effects.contains(&FrontendEffect::NeedsRender));
    }

    #[test]
    fn palette_change_emits_palette_changed() {
        // Mutating the live palette (as the `/theme` command does) is detected on
        // the next tick and reported so the frontend reloads chrome styling.
        let (mut driver, _srv_tx, _bs_tx) = FrontendDriver::for_test(fixture_core());
        let other = crate::theme::builtin_theme("dracula").unwrap();
        assert_ne!(&driver.palette, &other, "fixture must differ from dracula");
        driver.core_mut().palette = other;
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
