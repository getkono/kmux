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

pub use blink::{CURSOR_BLINK_HALF, advance_blink};
pub use clipboard::sanitize_clipboard_text;

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use kmux_client::connection_state::DisconnectReason;
use kmux_client::grid::CellGrid;
use kmux_client::supervisor::UpgradeSignal;
use kmux_client::transport::TransportKind;
use kmux_protocol::messages::{ClientMessage, KeyEvent, ServerMessage, TermSize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::core::{AppCore, BootstrapPhase, BootstrapTaskResult, KeyResult, SwitchTarget};
use crate::mode::{Action, Mode};

/// Liveness ping + timeout evaluation cadence.
const LIVENESS_TICK: Duration = Duration::from_secs(1);
/// Metrics JSONL flush cadence (see `docs/metrics.md`).
const METRICS_FLUSH_TICK: Duration = Duration::from_secs(10);
/// Debounce window for resize bursts; a window drag fires many size changes, so
/// coalesce them into one `set_term_size` after the burst settles.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(100);

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
    /// The `/theme` palette changed; reload toolkit-specific chrome styling.
    /// Implies a repaint. Read the new palette from [`FrontendDriver::palette`].
    PaletteChanged,
    /// Copy this (already NUL-sanitized) text to the system clipboard.
    CopyToClipboard(String),
    /// Read the system clipboard and feed it back via [`FrontendDriver::feed_paste`].
    RequestPaste,
    /// Exit the application.
    Quit,
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
    upgrade_rx: mpsc::Receiver<UpgradeSignal>,
    upgrade_tx: mpsc::Sender<UpgradeSignal>,
    /// SSH tunnel-death signal (the tunnel process exited unexpectedly).
    tunnel_died_rx: mpsc::Receiver<()>,
    tunnel_died_tx: mpsc::Sender<()>,
    /// Last palette applied, for `/theme` change detection.
    last_palette: crate::theme::Theme,
    /// Per-cadence bookkeeping: the pump fires on one interval, so each timer
    /// tracks its own last-fire / deadline.
    last_liveness: Instant,
    last_metrics_flush: Instant,
    pending_resize: Option<TermSize>,
    resize_deadline: Option<Instant>,
    /// Cursor-blink phase: `true` shows the cursor on the current frame.
    blink_on: bool,
    /// When the current blink half-cycle started; reset on keypress so typing
    /// shows a solid cursor.
    blink_phase_start: Instant,
}

impl FrontendDriver {
    /// Wrap an [`AppCore`], create the network channels, and kick off the initial
    /// bootstrap from `core.pending_target` (if any).
    ///
    /// Must be called with an ambient tokio runtime (`start_bootstrap` spawns).
    pub fn new(mut core: AppCore) -> Self {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (bs_tx, bs_rx) = mpsc::unbounded_channel::<BootstrapTaskResult>();
        let (upgrade_tx, upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
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
            upgrade_rx,
            upgrade_tx,
            tunnel_died_rx,
            tunnel_died_tx,
            last_palette,
            last_liveness: now,
            last_metrics_flush: now,
            pending_resize: None,
            resize_deadline: None,
            blink_on: true,
            blink_phase_start: now,
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

        if self.detect_palette_change() {
            effects.push(FrontendEffect::PaletteChanged);
            dirty = true;
        }
        dirty |= self.apply_settled_resize(now);
        dirty |= self.drain_server_messages(&mut effects);
        dirty |= self.poll_bootstrap_outcome();
        dirty |= self.drain_transport_upgrades();
        dirty |= self.drain_tunnel_deaths();
        dirty |= self.tick_liveness(now);
        self.tick_metrics(now);
        // Keep the HUD refreshing while shown (the metrics dialog is a snapshot
        // taken when it opens, so it does not need a per-frame tick).
        if self.core.hud_visible {
            dirty = true;
        }
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
        if dirty {
            effects.push(FrontendEffect::NeedsRender);
        }
        effects
    }

    /// Reflect a `/theme` palette change. The grid reads the palette live; this
    /// only flags the toolkit-specific chrome reload.
    fn detect_palette_change(&mut self) -> bool {
        if self.core.palette != self.last_palette {
            self.last_palette = self.core.palette.clone();
            true
        } else {
            false
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
            for m in batch {
                let events = self.core.mgr.handle_server_message(m);
                // Server-originated effects (OSC 52 clipboard writes) are
                // sanitized here so every frontend's clipboard write is safe.
                for eff in self.core.handle_session_events(events) {
                    if let KeyResult::CopyToClipboard(text) = eff {
                        effects.push(FrontendEffect::CopyToClipboard(
                            sanitize_clipboard_text(&text).into_owned(),
                        ));
                    }
                }
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
                self.core.reflect_bootstrap_outcome();
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
        let cursor_blinks = self.core.mgr.active_grid().is_some_and(|g| {
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
            KeyResult::SwitchServer(target) => self.switch_server(target),
            KeyResult::CopyToClipboard(text) => effects.push(FrontendEffect::CopyToClipboard(
                sanitize_clipboard_text(&text).into_owned(),
            )),
            KeyResult::RequestPaste => effects.push(FrontendEffect::RequestPaste),
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

    /// Apply a server-picker selection: `AppCore` mutates the server identity and
    /// returns the target; the driver rebuilds channels and bootstraps it.
    fn switch_server(&mut self, target: SwitchTarget) {
        let (srv_tx, srv_rx) = mpsc::unbounded_channel();
        self.srv_rx = srv_rx;
        let resolved = self.core.prepare_switch(&target);
        let (bs_tx, bs_rx) = mpsc::unbounded_channel();
        self.bootstrap_rx = Some(bs_rx);
        self.core
            .start_bootstrap(resolved, srv_tx, BootstrapPhase::Initial, bs_tx);
        self.core.needs_render = true;
    }

    /// Forward a batch of key events to the active pane's PTY, and reset the
    /// blink cycle so typing shows a solid cursor.
    pub fn send_keys(&mut self, keys: Vec<KeyEvent>) {
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
        self.core.mgr.send_paste(text);
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
        let (upgrade_tx, upgrade_rx) = mpsc::channel::<UpgradeSignal>(1);
        let (tunnel_died_tx, tunnel_died_rx) = mpsc::channel::<()>(1);
        let now = Instant::now();
        let last_palette = core.palette.clone();
        let driver = Self {
            core,
            srv_rx,
            bootstrap_rx: Some(bs_rx),
            upgrade_rx,
            upgrade_tx,
            tunnel_died_rx,
            tunnel_died_tx,
            last_palette,
            last_liveness: now,
            last_metrics_flush: now,
            pending_resize: None,
            resize_deadline: None,
            blink_on: true,
            blink_phase_start: now,
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
}
