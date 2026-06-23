//! Closed-session graveyard management (issue #64).
//!
//! When a session is closed it is not discarded — its snapshot is retained here
//! so the user can restore it later. The set is bounded by a count cap and an
//! age (TTL) cap, both configurable. It is persisted to its own `closed.bin`
//! file (see `persist/graveyard.rs`), rewritten only when the set changes:
//!
//! - **close** (see `app/crud.rs`): snapshot → [`retain_closed_session`] →
//!   *then* kill the processes, so the snapshot is durable even across a crash.
//! - **restore** (see `app/restore.rs`): the entry is removed and the file
//!   rewritten.
//! - **TTL sweep**: [`sweep_graveyard`] runs on the periodic checkpoint tick and
//!   rewrites the file only if it actually pruned something (so a steady state
//!   costs zero extra I/O).

use kmux_protocol::messages::{ClosedSessionEntry, SessionEntry, epoch_millis};
use kmux_pty::error::{KmuxError, Result};
use tracing::warn;

use crate::persist::{GRAVEYARD_VERSION, PersistedClosedSession, PersistedGraveyard};

use super::ServerApp;
use super::persistence::RestoreReport;

impl ServerApp {
    /// Retain a just-closed session in the graveyard: push it, enforce the caps,
    /// and persist the file. Called by `close_session` *before* the PTYs are
    /// killed so the snapshot is on disk even if the daemon then crashes.
    pub(super) fn retain_closed_session(&self, closed: PersistedClosedSession) {
        self.closed_sessions.lock().unwrap().push(closed);
        self.prune_graveyard();
        self.persist_graveyard();
    }

    /// Apply both retention caps to the in-memory graveyard, releasing the word
    /// of every evicted session back to the pool. Returns `true` if anything was
    /// removed (so the caller knows whether the file needs rewriting).
    pub(super) fn prune_graveyard(&self) -> bool {
        let now = epoch_millis();
        let mut evicted: Vec<String> = Vec::new();
        {
            let mut guard = self.closed_sessions.lock().unwrap();

            // Age cap: drop entries older than the TTL (0 disables it).
            if self.closed_session_ttl_ms > 0 {
                guard.retain(|c| {
                    let keep = c.closed_at_ms.saturating_add(self.closed_session_ttl_ms) >= now;
                    if !keep {
                        evicted.push(c.session.meta.word_id.clone());
                    }
                    keep
                });
            }

            // Count cap: keep the newest `keep`, evicting the oldest (front).
            if guard.len() > self.closed_session_keep {
                let remove = guard.len() - self.closed_session_keep;
                for c in guard.drain(0..remove) {
                    evicted.push(c.session.meta.word_id.clone());
                }
            }
        }

        if evicted.is_empty() {
            return false;
        }
        let mut wl = self.wordlist.lock().unwrap();
        for word in &evicted {
            wl.release(word);
        }
        true
    }

    /// Snapshot the graveyard as wire [`ClosedSessionEntry`]s for the restore
    /// UI, ordered most-recently-active first (issue #64).
    pub(crate) fn closed_session_entries(&self) -> Vec<ClosedSessionEntry> {
        let mut entries: Vec<ClosedSessionEntry> = self
            .closed_sessions
            .lock()
            .unwrap()
            .iter()
            .map(|c| ClosedSessionEntry {
                meta: c.session.meta.clone(),
                last_active_ms: c.session.last_active_ms,
                closed_at_ms: c.closed_at_ms,
                pane_count: c.session.panes.len() as u32,
            })
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_active_ms));
        entries
    }

    /// Restore a closed session from the graveyard (issue #64): respawn its
    /// panes (replaying scrollback), move it into the live map, and rewrite the
    /// graveyard file. The word was already reserved while inactive, so it
    /// carries over to the live session unchanged.
    ///
    /// A failed restore leaves the snapshot in the graveyard (and its word
    /// reserved) so the user can retry; the entry is only removed on success.
    pub(crate) async fn restore_session(&self, word_id: &str) -> Result<SessionEntry> {
        let snapshot = {
            let guard = self.closed_sessions.lock().unwrap();
            guard
                .iter()
                .find(|c| c.session.meta.word_id == word_id)
                .map(|c| c.session.clone())
        };
        let Some(snapshot) = snapshot else {
            return Err(KmuxError::SessionNotFound {
                name: word_id.to_string(),
            });
        };

        // Graveyard sessions always respawn (no inherited live fds).
        let mut report = RestoreReport::default();
        let mut inherited = std::collections::HashMap::new();
        let Some(state) = self
            .restore_one_session(snapshot, &mut inherited, &mut report)
            .await
        else {
            return Err(KmuxError::Spawn(format!(
                "failed to restore closed session '{word_id}'"
            )));
        };

        let entry = self.build_session_entry(&state);
        self.sessions
            .write()
            .await
            .insert(word_id.to_string(), state);

        // Success: drop the entry and rewrite the file. The reserved word now
        // belongs to the live session.
        self.closed_sessions
            .lock()
            .unwrap()
            .retain(|c| c.session.meta.word_id != word_id);
        self.persist_graveyard();

        Ok(entry)
    }

    /// Prune by TTL/count and rewrite the graveyard file only if it changed.
    /// Invoked on the periodic checkpoint tick — cheap when nothing expires.
    pub(crate) fn sweep_graveyard(&self) {
        if self.prune_graveyard() {
            self.persist_graveyard();
        }
    }

    /// Atomically write the current graveyard to disk. A no-op when no graveyard
    /// path is configured (e.g. in tests, where the set is in memory only).
    pub(crate) fn persist_graveyard(&self) {
        let Some(path) = self.graveyard_path.as_ref() else {
            return;
        };
        let graveyard = {
            let guard = self.closed_sessions.lock().unwrap();
            PersistedGraveyard {
                version: GRAVEYARD_VERSION,
                sessions: guard.clone(),
            }
        };
        if let Err(e) = crate::persist::graveyard::write_graveyard(&graveyard, path) {
            warn!("failed to persist closed-session graveyard: {e}");
        }
    }

    /// Install a graveyard loaded from disk at startup.
    ///
    /// Drops entries that are expired or that collide with a live session (live
    /// wins — this can happen if the daemon crashed after writing the graveyard
    /// but before the next live checkpoint dropped the session). Reserves each
    /// surviving word so new sessions never reuse a restorable session's id.
    /// Returns `true` if anything was dropped, so the caller can rewrite the file.
    pub(crate) async fn load_graveyard(&self, graveyard: PersistedGraveyard) -> bool {
        let now = epoch_millis();
        let live: std::collections::HashSet<String> =
            self.sessions.read().await.keys().cloned().collect();

        let original = graveyard.sessions.len();
        let mut kept: Vec<PersistedClosedSession> = graveyard
            .sessions
            .into_iter()
            .filter(|c| {
                let word = &c.session.meta.word_id;
                if live.contains(word) {
                    return false;
                }
                if self.closed_session_ttl_ms > 0
                    && c.closed_at_ms.saturating_add(self.closed_session_ttl_ms) < now
                {
                    return false;
                }
                true
            })
            .collect();
        // Maintain the oldest-first invariant the count cap relies on.
        kept.sort_by_key(|c| c.closed_at_ms);
        if kept.len() > self.closed_session_keep {
            let remove = kept.len() - self.closed_session_keep;
            kept.drain(0..remove);
        }

        {
            let mut wl = self.wordlist.lock().unwrap();
            for c in &kept {
                wl.reserve(&c.session.meta.word_id);
            }
        }

        let changed = kept.len() != original;
        *self.closed_sessions.lock().unwrap() = kept;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::{PersistedPane, PersistedSession, PersistedTab, PersistedTermSize};
    use kmux_protocol::messages::{
        CursorState, GridSnapshot, LayoutNode, SessionMeta, SessionStatus, TermModes,
    };

    fn closed(word: &str, closed_at_ms: u64) -> PersistedClosedSession {
        PersistedClosedSession {
            closed_at_ms,
            session: PersistedSession {
                meta: SessionMeta {
                    index: 0,
                    word_id: word.to_string(),
                    name: word.to_string(),
                    cwd: "/tmp".to_string(),
                },
                next_pane_index: 1,
                panes: vec![PersistedPane {
                    pane_index: 0,
                    program: "/bin/sh".to_string(),
                    args: vec![],
                    size: PersistedTermSize { rows: 24, cols: 80 },
                    status: SessionStatus::Running,
                    child_pid: None,
                    grid: GridSnapshot {
                        rows: 24,
                        cols: 80,
                        cells: vec![Default::default(); 24 * 80],
                        cursor: CursorState::default(),
                        modes: TermModes::EMPTY,
                        history_total: 0,
                        scrollback_base: 0,
                        scrollback_tail: Vec::new(),
                    },
                    scrollback_lines: vec![],
                    cwd: "/tmp".to_string(),
                }],
                tabs: vec![PersistedTab {
                    tab_index: 0,
                    name: "1".to_string(),
                    layout: LayoutNode::single(0),
                    focused_pane: 0,
                }],
                next_tab_index: 1,
                active_tab: 0,
                last_active_ms: closed_at_ms,
            },
        }
    }

    fn words(app: &ServerApp) -> Vec<String> {
        app.closed_sessions
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.session.meta.word_id.clone())
            .collect()
    }

    #[test]
    fn count_cap_evicts_oldest() {
        let mut app = ServerApp::new("tok".to_string());
        app.closed_session_keep = 2;
        app.closed_session_ttl_ms = 0; // disable TTL for this test

        // Reserve the words first so the eviction-time release is balanced.
        for w in ["eagle", "falcon", "hawk"] {
            app.wordlist.lock().unwrap().reserve(w);
        }
        app.retain_closed_session(closed("eagle", 100));
        app.retain_closed_session(closed("falcon", 200));
        app.retain_closed_session(closed("hawk", 300));

        // Oldest ("eagle") evicted; the two newest remain.
        assert_eq!(words(&app), vec!["falcon", "hawk"]);
    }

    #[test]
    fn ttl_prunes_stale_entries() {
        let mut app = ServerApp::new("tok".to_string());
        app.closed_session_keep = 100;
        app.closed_session_ttl_ms = 1000; // 1 second

        let now = epoch_millis();
        app.closed_sessions
            .lock()
            .unwrap()
            .push(closed("old", now - 5_000)); // 5 s ago → expired
        app.closed_sessions
            .lock()
            .unwrap()
            .push(closed("fresh", now)); // now → kept

        let removed = app.prune_graveyard();
        assert!(removed, "TTL prune should have removed the stale entry");
        assert_eq!(words(&app), vec!["fresh"]);
    }

    #[test]
    fn prune_noop_when_within_caps() {
        let mut app = ServerApp::new("tok".to_string());
        app.closed_session_keep = 10;
        app.closed_session_ttl_ms = 0;
        app.closed_sessions
            .lock()
            .unwrap()
            .push(closed("eagle", epoch_millis()));
        assert!(!app.prune_graveyard(), "nothing to prune → no change");
    }

    #[tokio::test]
    async fn close_then_restore_roundtrip() {
        use kmux_protocol::messages::{ClientCapabilities, TermSize};

        let app = ServerApp::new("tok".to_string());
        let entry = app
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/sleep".to_string()),
                vec!["30".to_string()],
                TermSize::default(),
                &ClientCapabilities::default(),
            )
            .await
            .expect("create_session");
        let word = entry.meta.word_id.clone();

        // Close → session leaves the live map and lands in the graveyard.
        app.close_session(&word).await.expect("close_session");
        assert!(app.sessions.read().await.get(&word).is_none());
        assert_eq!(words(&app), vec![word.clone()]);

        // Restore → session is live again and gone from the graveyard.
        let restored = app.restore_session(&word).await.expect("restore_session");
        assert_eq!(restored.meta.word_id, word);
        assert!(app.sessions.read().await.get(&word).is_some());
        assert!(app.closed_sessions.lock().unwrap().is_empty());

        // Restoring a word that isn't in the graveyard errors.
        assert!(app.restore_session("nonexistent").await.is_err());

        let _ = app.close_session(&word).await; // clean up the spawned child
    }

    #[tokio::test]
    async fn load_drops_expired_and_reserves_words() {
        let mut app = ServerApp::new("tok".to_string());
        app.closed_session_keep = 100;
        app.closed_session_ttl_ms = 1000;

        let now = epoch_millis();
        let gy = PersistedGraveyard {
            version: GRAVEYARD_VERSION,
            sessions: vec![closed("stale", now - 9_999), closed("fresh", now)],
        };
        let changed = app.load_graveyard(gy).await;
        assert!(changed, "expired entry dropped → file needs rewrite");
        assert_eq!(words(&app), vec!["fresh"]);
        // The surviving word is already reserved (reserve returns false), so a
        // new session won't reuse it.
        assert!(
            !app.wordlist.lock().unwrap().reserve("fresh"),
            "surviving word should already be reserved"
        );
    }
}
