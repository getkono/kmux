use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{KmuxError, Result};
use crate::events::{EventBus, SessionEvent};
use crate::process::ExitStatus;
use crate::session::PtySession;

/// Manages a collection of named PTY sessions.
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<String, PtySession>>>,
    events: EventBus,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events: EventBus::new(256),
        }
    }

    /// Create a new session manager with a custom event bus.
    pub fn with_events(events: EventBus) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }

    /// Subscribe to session lifecycle events.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Spawn a new named session.
    pub async fn spawn(&self, name: impl Into<String>, config: &PtyConfig) -> Result<()> {
        let name = name.into();
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&name) {
            return Err(KmuxError::SessionAlreadyExists { name });
        }
        let session = PtySession::spawn(config)?;
        self.events
            .emit(SessionEvent::Spawned { name: name.clone() });
        sessions.insert(name, session);
        Ok(())
    }

    /// Register an already-constructed `PtySession` under a name.
    ///
    /// Used when adopting a live PTY handed off from a previous daemon: the
    /// `PtySession` was built from [`crate::PtyProcess::from_inherited`] and needs to be
    /// tracked by the registry so that `get_session`, `resize`, and `close` work
    /// normally.
    pub async fn register(&self, name: impl Into<String>, session: PtySession) -> Result<()> {
        let name = name.into();
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&name) {
            return Err(KmuxError::SessionAlreadyExists { name });
        }
        self.events
            .emit(SessionEvent::Spawned { name: name.clone() });
        sessions.insert(name, session);
        Ok(())
    }

    /// Close and remove a named session.
    pub async fn close(&self, name: &str) -> Result<ExitStatus> {
        let session = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .remove(name)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: name.to_string(),
                })?
        };
        let status = session.close().await?;
        self.events.emit(SessionEvent::Closed {
            name: name.to_string(),
        });
        Ok(status)
    }

    /// Remove a named session and initiate graceful shutdown in the background.
    ///
    /// Returns immediately without waiting for the process to exit. The process
    /// receives SIGTERM and will be `SIGKILLed` after the grace period if needed.
    pub async fn close_nowait(&self, name: &str) -> Result<()> {
        let session = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .remove(name)
                .ok_or_else(|| KmuxError::SessionNotFound {
                    name: name.to_string(),
                })?
        };
        session.close_nowait().await;
        self.events.emit(SessionEvent::Closed {
            name: name.to_string(),
        });
        Ok(())
    }

    /// List all active session names.
    pub async fn list(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    /// Check if a session exists.
    pub async fn exists(&self, name: &str) -> bool {
        self.sessions.lock().await.contains_key(name)
    }

    /// Return the number of active sessions.
    pub async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Return true if there are no active sessions.
    pub async fn is_empty(&self) -> bool {
        self.sessions.lock().await.is_empty()
    }

    /// Get a clone of a named session handle for direct I/O.
    ///
    /// The returned `PtySession` shares the same underlying PTY process via
    /// `Arc`. The session remains alive as long as at least one handle exists.
    pub async fn get_session(&self, name: &str) -> Result<PtySession> {
        self.sessions
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: name.to_string(),
            })
    }

    /// Resize the PTY window for a named session.
    pub async fn resize(&self, name: &str, size: WindowSize) -> Result<()> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(name)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: name.to_string(),
            })?;
        session.resize(size).await?;
        self.events.emit(SessionEvent::Resized {
            name: name.to_string(),
            rows: size.rows,
            cols: size.cols,
        });
        Ok(())
    }

    /// Duplicate the master fd of a named session for transfer to a successor
    /// daemon via `SCM_RIGHTS`. The child stays alive as long as either the
    /// original or the dup remains open.
    pub async fn dup_master_fd(&self, name: &str) -> Result<std::os::fd::OwnedFd> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(name)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: name.to_string(),
            })?;
        session.dup_master_fd().await
    }

    /// Whether a named session's child process has exited.
    ///
    /// Returns `None` if the session does not exist, `Some(true)` if its child
    /// has exited, `Some(false)` if it is still running.
    pub async fn is_exited(&self, name: &str) -> Option<bool> {
        let sessions = self.sessions.lock().await;
        match sessions.get(name) {
            Some(session) => Some(session.is_exited().await),
            None => None,
        }
    }

    /// Return the PID of the child process for a named session.
    ///
    /// Returns `None` if the session does not exist.
    pub async fn child_pid(&self, name: &str) -> Option<nix::unistd::Pid> {
        let sessions = self.sessions.lock().await;
        match sessions.get(name) {
            Some(session) => Some(session.child_pid().await),
            None => None,
        }
    }

    /// Emit a [`SessionEvent::Exited`] for a session whose child has been
    /// observed to exit (e.g. the relay loop detected PTY EOF).
    ///
    /// Unlike [`close`](Self::close) this does *not* remove the session from the
    /// registry — it only notifies subscribers. This is the path that surfaces a
    /// naturally-exiting shell (and an exited foreign child inherited across a
    /// daemon handoff, which cannot be `waitpid`-ed) to attached clients.
    pub fn notify_exited(&self, name: &str, status: ExitStatus) {
        self.events.emit(SessionEvent::Exited {
            name: name.to_string(),
            status,
        });
    }

    /// Set keep-alive mode on all active sessions.
    ///
    /// Called on clean daemon shutdown so that child PTY processes remain
    /// alive for reattachment when the daemon restarts.
    pub async fn set_all_keep_alive(&self, val: bool) {
        let sessions = self.sessions.lock().await;
        for session in sessions.values() {
            session.set_keep_alive(val).await;
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_and_list() {
        let mgr = SessionManager::new();
        let config = PtyConfig::new("/bin/sleep").args(["10"]);
        mgr.spawn("alpha", &config).await.expect("spawn");
        assert!(mgr.exists("alpha").await);
        assert_eq!(mgr.len().await, 1);
        mgr.close("alpha").await.expect("close");
        assert!(!mgr.exists("alpha").await);
    }

    #[tokio::test]
    async fn duplicate_name_errors() {
        let mgr = SessionManager::new();
        let config = PtyConfig::new("/bin/sleep").args(["10"]);
        mgr.spawn("beta", &config).await.expect("first spawn");
        let result = mgr.spawn("beta", &config).await;
        assert!(result.is_err());
        mgr.close("beta").await.expect("cleanup");
    }

    #[tokio::test]
    async fn close_nonexistent_errors() {
        let mgr = SessionManager::new();
        let result = mgr.close("ghost").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn close_nowait_removes_session_immediately() {
        let mgr = SessionManager::new();
        let config = PtyConfig::new("/bin/sleep").args(["999"]);
        mgr.spawn("gamma", &config).await.expect("spawn");
        assert!(mgr.exists("gamma").await);

        let start = std::time::Instant::now();
        mgr.close_nowait("gamma").await.expect("close_nowait");
        let elapsed = start.elapsed();

        // Should return well under the 5-second grace period
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "close_nowait took {elapsed:?}, expected < 200ms"
        );
        assert!(!mgr.exists("gamma").await, "session should be removed");
    }

    #[tokio::test]
    async fn close_nowait_nonexistent_errors() {
        let mgr = SessionManager::new();
        let result = mgr.close_nowait("ghost").await;
        assert!(result.is_err());
    }
}
