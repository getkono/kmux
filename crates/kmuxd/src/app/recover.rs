//! Respawn of crashed isolated VT workers (issue #126).
//!
//! When a worker subprocess dies abnormally, its supervisor reports the pane id
//! on [`ServerApp::worker_fault_tx`]. A single daemon task — spawned once by
//! [`ServerApp::spawn_worker_respawn_task`] — drains those reports and respawns
//! the worker out of band (never re-entrantly from the dying supervisor). The
//! shell survived the crash (the daemon holds the PTY master fd), so a fresh
//! worker re-adopts that fd and the pane becomes usable again.
//!
//! A crash-loop guard bounds restarts to [`MAX_RESTARTS`] within
//! [`RESTART_WINDOW`]; past that the pane is left faulted. A worker killed
//! for leaving a heartbeat unanswered (a hang, issue #207) counts against a
//! budget of its own instead — [`MAX_HANG_RESTARTS`] within [`HANG_WINDOW`] —
//! since a hang takes at least the heartbeat deadline to detect, and says
//! nothing about the emulator crashing. A respawn waits out a graceful
//! handoff (it would read a PTY the handoff has frozen), and none happens
//! once one has committed. The respawned
//! emulator starts blank (the shell is alive but won't redraw on its own), so
//! attached clients are reset with a forced snapshot; preserving the pre-crash
//! screen across a respawn is a follow-up (see `docs/architecture-process-isolation.md`).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use kmux_protocol::messages::{SequenceNo, ServerMessage, epoch_millis};
use tracing::{info, warn};

use super::PaneEventSink;
use super::ServerApp;
use super::helpers::{get_pane_relay, get_pane_relay_mut};
use crate::engine::{FaultCause, WorkerFault};

/// Max worker respawns after a crash for one pane within [`RESTART_WINDOW`]
/// before giving up.
const MAX_RESTARTS: usize = 3;
/// Sliding window for the crash budget.
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Max worker respawns after a hang for one pane within [`HANG_WINDOW`].
const MAX_HANG_RESTARTS: usize = 3;
/// Sliding window for the hang budget: long enough to hold several hangs,
/// each of which takes the heartbeat deadline to detect.
const HANG_WINDOW: Duration = Duration::from_secs(600);

/// One pane's worker respawns, by cause, each against its own budget.
#[derive(Debug, Default)]
pub(super) struct RestartLog {
    crashes: Vec<Instant>,
    hangs: Vec<Instant>,
}

impl RestartLog {
    /// Record a respawn for `cause` at `now`, if its budget allows one.
    fn allow(&mut self, cause: FaultCause, now: Instant) -> bool {
        let (log, max, window) = match cause {
            FaultCause::Crash => (&mut self.crashes, MAX_RESTARTS, RESTART_WINDOW),
            FaultCause::Hang => (&mut self.hangs, MAX_HANG_RESTARTS, HANG_WINDOW),
        };
        log.retain(|t| now.duration_since(*t) < window);
        if log.len() >= max {
            return false;
        }
        log.push(now);
        true
    }

    /// The respawns within their windows at `now`, the age of the newest,
    /// and whether both budgets still allow another.
    fn stats(&self, now: Instant) -> (usize, Option<Duration>, bool) {
        let recent = |log: &[Instant], window: Duration| -> Vec<Instant> {
            log.iter()
                .copied()
                .filter(|t| now.duration_since(*t) < window)
                .collect()
        };
        let crashes = recent(&self.crashes, RESTART_WINDOW);
        let hangs = recent(&self.hangs, HANG_WINDOW);
        let newest = crashes.iter().chain(&hangs).max();
        let within = crashes.len() < MAX_RESTARTS && hangs.len() < MAX_HANG_RESTARTS;
        (
            crashes.len() + hangs.len(),
            newest.map(|t| now.duration_since(*t)),
            within,
        )
    }
}

impl ServerApp {
    /// Spawn the background task that respawns crashed workers, restarted if
    /// it panics (issue #206). Call once, after the `ServerApp` is wrapped in
    /// its `Arc`.
    pub(crate) fn spawn_worker_respawn_task(self: &Arc<Self>) {
        let Some(rx) = self.worker_fault_rx.lock().unwrap().take() else {
            return; // already started
        };
        // Shared so a restarted run picks up the same fault channel; an async
        // mutex is released, not poisoned, when a run panics.
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let app = Arc::clone(self);
        tokio::spawn(crate::supervisor::supervise("worker-respawn", move || {
            let rx = Arc::clone(&rx);
            let app = Arc::clone(&app);
            async move {
                let mut rx = rx.lock().await;
                while let Some(fault) = rx.recv().await {
                    app.on_worker_fault(&fault).await;
                }
            }
        }));
    }

    /// Respawn the isolated worker for a faulted pane, re-adopting the live PTY.
    async fn recover_faulted_worker(self: &Arc<Self>, pane_id: &str) {
        // Snapshot the handles needed to rebuild the engine under a short read
        // lock; bail if the pane vanished or isn't worker-isolated.
        let gathered = {
            let sessions = self.sessions.read().await;
            let Ok(relay) = get_pane_relay(&sessions, pane_id) else {
                return;
            };
            if !relay.engine.is_worker() {
                return;
            }
            (
                relay.size,
                relay.kitty_graphics_enabled.load(Ordering::Relaxed),
                relay.kitty_keyboard_enabled.load(Ordering::Relaxed),
                relay.clients.clone(),
                relay.scrollback.clone(),
                relay.seqno_counter.clone(),
                relay.title.clone(),
                relay.progress.clone(),
            )
        };
        let (
            size,
            kitty_graphics,
            kitty_keyboard,
            clients,
            scrollback,
            seqno_counter,
            title,
            progress,
        ) = gathered;

        // Spawn the replacement worker WITHOUT holding the sessions lock (the
        // handshake is async). It re-adopts the daemon's retained master fd.
        let Ok(session) = self.manager.get_session(pane_id).await else {
            return;
        };
        let event_sink = Arc::new(PaneEventSink::new(
            pane_id.to_string(),
            title,
            progress,
            self.vt_events_tx.clone(),
        ));
        let new_engine = match self
            .try_spawn_worker_engine(
                pane_id,
                size,
                kitty_graphics,
                kitty_keyboard,
                &session,
                clients,
                scrollback,
                seqno_counter,
                event_sink,
            )
            .await
        {
            Ok(engine) => engine,
            Err(e) => {
                warn!(pane_id, "worker respawn failed: {e}");
                return;
            }
        };

        // Swap in the fresh engine, re-checking the pane still exists.
        {
            let mut sessions = self.sessions.write().await;
            let Ok(relay) = get_pane_relay_mut(&mut sessions, pane_id) else {
                return; // pane closed mid-respawn; drop the new engine
            };
            relay.engine = new_engine;
        }

        // The new emulator is blank; reset attached clients to it so their grids
        // do not diverge. Live shell output repaints from here.
        self.force_pane_resync(pane_id).await;
        info!(pane_id, "respawned isolated VT worker after crash");
    }

    /// Act on a worker fault: respawn the pane's worker if the budget for
    /// its cause allows — once no graceful handoff runs, and never after one
    /// committed.
    async fn on_worker_fault(self: &Arc<Self>, fault: &WorkerFault) {
        let Some(admitted) = self.wait_for_pane_admission().await else {
            return; // handed off: this daemon is exiting
        };
        if self.allow_worker_restart(&fault.pane_id, fault.cause) {
            self.recover_faulted_worker(&fault.pane_id).await;
        } else {
            warn!(
                pane_id = %fault.pane_id,
                cause = ?fault.cause,
                "worker restart budget exceeded; pane stays faulted"
            );
        }
        drop(admitted);
    }

    /// Push a fresh full snapshot (from the pane's engine) to every non-paused
    /// attached client, resetting their grid.
    async fn force_pane_resync(&self, pane_id: &str) {
        let sessions = self.sessions.read().await;
        let Ok(relay) = get_pane_relay(&sessions, pane_id) else {
            return;
        };
        let snapshot = Arc::new(relay.engine.snapshot());
        let seqno = SequenceNo(relay.seqno_counter.fetch_add(1, Ordering::Relaxed));
        let msg = ServerMessage::TerminalSnapshot {
            pane_id: pane_id.to_string(),
            snapshot,
            seqno,
            sent_at_ms: epoch_millis(),
        };
        for sender in relay.clients.lock().unwrap().values() {
            // A paused client skips the post-respawn snapshot and resyncs on
            // resume; an auto-pause-exempt pane still streams, so it gets it.
            if !sender.output_paused() {
                let _ = sender.data_tx.try_send(msg.clone());
            }
        }
    }

    /// Record a restart attempt for `cause` and report whether it is within
    /// that cause's budget.
    fn allow_worker_restart(&self, pane_id: &str, cause: FaultCause) -> bool {
        self.worker_restart_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(pane_id.to_string())
            .or_default()
            .allow(cause, Instant::now())
    }

    /// Drop `pane_id`'s restart history once the pane is closed (issue #207).
    /// The log gains an entry per pane that ever faulted; without this a
    /// daemon that runs for months keeps one for every pane it ever closed.
    pub(super) fn forget_worker_restarts(&self, pane_id: &str) {
        self.worker_restart_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(pane_id);
    }

    /// Read-only view of the restart budgets for `pane_id`, for status
    /// reporting: respawns recorded within their live windows (crashes and
    /// hangs together), the age of the most recent, and whether both budgets
    /// still allow another. Mirrors [`Self::allow_worker_restart`]'s windowing
    /// without mutating the log.
    pub(super) fn worker_restart_stats(&self, pane_id: &str) -> (usize, Option<Duration>, bool) {
        self.worker_restart_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .map_or((0, None, true), |log| log.stats(Instant::now()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kmux_protocol::messages::{ClientCapabilities, TermSize};

    use super::{HANG_WINDOW, MAX_HANG_RESTARTS, MAX_RESTARTS, RESTART_WINDOW, RestartLog};
    use crate::engine::FaultCause;

    /// A fault is recorded against its cause's budget before any respawn,
    /// and waits out a handoff in flight (issue #207).
    #[tokio::test]
    async fn a_fault_is_counted_once_no_handoff_runs() {
        let app = Arc::new(crate::fixtures::fixture_app());
        let fault = crate::engine::WorkerFault {
            pane_id: "eagle/0".into(),
            cause: FaultCause::Hang,
        };
        let closed = app.close_pane_creation().await;
        let waiting = tokio::spawn({
            let app = Arc::clone(&app);
            let fault = fault.clone();
            async move { app.on_worker_fault(&fault).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(app.worker_restart_stats("eagle/0").0, 0, "waits");
        drop(closed);
        tokio::time::timeout(std::time::Duration::from_secs(10), waiting)
            .await
            .expect("admitted once the handoff ends")
            .unwrap();
        assert_eq!(app.worker_restart_stats("eagle/0").0, 1);

        app.keep_pane_creation_closed(app.close_pane_creation().await);
        app.on_worker_fault(&fault).await;
        assert_eq!(app.worker_restart_stats("eagle/0").0, 1, "handed off");
    }

    /// Crashes and hangs each have their own budget: hangs never use up the
    /// crash budget, nor crashes the hang one, and each budget frees up as
    /// its window passes (issue #207).
    #[test]
    fn crashes_and_hangs_have_budgets_of_their_own() {
        let start = std::time::Instant::now();
        let mut log = RestartLog::default();
        for _ in 0..MAX_HANG_RESTARTS {
            assert!(log.allow(FaultCause::Hang, start));
        }
        assert!(!log.allow(FaultCause::Hang, start), "hang budget spent");
        assert!(!log.stats(start).2, "not within budget");
        for _ in 0..MAX_RESTARTS {
            assert!(log.allow(FaultCause::Crash, start), "crashes unaffected");
        }
        assert!(!log.allow(FaultCause::Crash, start), "crash budget spent");
        assert_eq!(log.stats(start).0, MAX_RESTARTS + MAX_HANG_RESTARTS);

        let mut crashes_only = RestartLog::default();
        for _ in 0..MAX_RESTARTS {
            assert!(crashes_only.allow(FaultCause::Crash, start));
        }
        assert!(
            !crashes_only.stats(start).2,
            "the crash budget alone is spent"
        );

        let crashes_expired = start + RESTART_WINDOW;
        // A respawn a whole window old no longer counts, for either cause.
        assert_eq!(
            log.stats(crashes_expired).0,
            MAX_HANG_RESTARTS,
            "crashes expired"
        );
        assert_eq!(log.stats(start + HANG_WINDOW).0, 0, "hangs expired too");
        assert!(log.allow(FaultCause::Crash, crashes_expired));
        assert!(
            !log.allow(FaultCause::Hang, crashes_expired),
            "a hang lasts longer"
        );
        let hangs_expired = start + HANG_WINDOW;
        assert!(log.allow(FaultCause::Hang, hangs_expired));
        let (count, newest, within) = log.stats(hangs_expired);
        assert_eq!(count, 1, "only the hang just recorded");
        assert_eq!(newest, Some(std::time::Duration::ZERO));
        assert!(within);
    }

    /// Closing a pane, a tab or a session forgets the restart history of the
    /// panes it closed (issue #207), and only theirs.
    #[tokio::test]
    async fn closing_a_pane_forgets_its_worker_restarts() {
        let app = crate::fixtures::fixture_app();
        let size = TermSize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        };
        let caps = ClientCapabilities::default();
        let mut pane_ids = Vec::new();
        for _ in 0..3 {
            let entry = app
                .create_session(None, None, Some("/bin/cat".into()), vec![], size, &caps)
                .await
                .expect("a session");
            let pane_id = kmux_protocol::format_pane_id(&entry.meta.word_id, 0);
            assert!(app.allow_worker_restart(&pane_id, FaultCause::Crash));
            assert_eq!(app.worker_restart_stats(&pane_id).0, 1);
            pane_ids.push((entry.meta.word_id, pane_id));
        }
        let [(_, by_pane), (word, by_tab), (session, by_session)] =
            <[_; 3]>::try_from(pane_ids).unwrap();

        app.close_pane(&by_pane).await.expect("close the pane");
        assert_eq!(app.worker_restart_stats(&by_pane).0, 0);
        assert_eq!(app.worker_restart_stats(&by_tab).0, 1, "others kept");

        app.close_tab(&word, 0).await.expect("close the tab");
        assert_eq!(app.worker_restart_stats(&by_tab).0, 0);

        app.close_session(&session)
            .await
            .expect("close the session");
        assert_eq!(app.worker_restart_stats(&by_session).0, 0);
    }

    /// The respawn task takes the fault channel, so a second call is a no-op
    /// rather than a second consumer.
    #[tokio::test]
    async fn spawning_the_respawn_task_takes_the_fault_channel() {
        let app = Arc::new(crate::fixtures::fixture_app());
        app.spawn_worker_respawn_task();
        assert!(app.worker_fault_rx.lock().unwrap().is_none());
    }
}
