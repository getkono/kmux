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
//! [`RESTART_WINDOW`]; past that the pane is left faulted. The respawned
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

/// Max worker respawns for one pane within [`RESTART_WINDOW`] before giving up.
const MAX_RESTARTS: usize = 3;
/// Sliding window for the restart budget.
const RESTART_WINDOW: Duration = Duration::from_secs(60);

impl ServerApp {
    /// Spawn the background task that respawns crashed workers. Call once, after
    /// the `ServerApp` is wrapped in its `Arc`.
    pub(crate) fn spawn_worker_respawn_task(self: &Arc<Self>) {
        let Some(mut rx) = self.worker_fault_rx.lock().unwrap().take() else {
            return; // already started
        };
        let app = Arc::clone(self);
        tokio::spawn(async move {
            while let Some(pane_id) = rx.recv().await {
                app.recover_faulted_worker(&pane_id).await;
            }
        });
    }

    /// Respawn the isolated worker for a faulted pane, re-adopting the live PTY.
    async fn recover_faulted_worker(self: &Arc<Self>, pane_id: &str) {
        if !self.allow_worker_restart(pane_id) {
            warn!(
                pane_id,
                "worker restart budget exceeded; pane stays faulted"
            );
            return;
        }

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

    /// Push a fresh full snapshot (from the pane's engine) to every non-paused
    /// attached client, resetting their grid.
    async fn force_pane_resync(&self, pane_id: &str) {
        let sessions = self.sessions.read().await;
        let Ok(relay) = get_pane_relay(&sessions, pane_id) else {
            return;
        };
        let snapshot = std::sync::Arc::new(relay.engine.snapshot());
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

    /// Record a restart attempt and report whether it is within budget.
    fn allow_worker_restart(&self, pane_id: &str) -> bool {
        let now = Instant::now();
        let mut log = self.worker_restart_log.lock().unwrap();
        let entry = log.entry(pane_id.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < RESTART_WINDOW);
        if entry.len() >= MAX_RESTARTS {
            return false;
        }
        entry.push(now);
        true
    }

    /// Read-only view of the crash-loop budget for `pane_id`, for status
    /// reporting: respawns recorded within the live [`RESTART_WINDOW`], the age
    /// of the most recent, and whether another respawn is still allowed. Mirrors
    /// [`Self::allow_worker_restart`]'s windowing without mutating the log.
    pub(super) fn worker_restart_stats(&self, pane_id: &str) -> (usize, Option<Duration>, bool) {
        let now = Instant::now();
        let log = self.worker_restart_log.lock().unwrap();
        let Some(entry) = log.get(pane_id) else {
            return (0, None, true);
        };
        let mut count = 0usize;
        let mut newest: Option<Instant> = None;
        for &t in entry.iter() {
            if now.duration_since(t) < RESTART_WINDOW {
                count += 1;
                newest = Some(newest.map_or(t, |n| n.max(t)));
            }
        }
        let last_age = newest.map(|t| now.duration_since(t));
        (count, last_age, count < MAX_RESTARTS)
    }
}
