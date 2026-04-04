use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::{PtyConfig, WindowSize};
use crate::error::{Result, kmuxError};
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
            return Err(kmuxError::SessionAlreadyExists { name });
        }
        let session = PtySession::spawn(config)?;
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
                .ok_or_else(|| kmuxError::SessionNotFound {
                    name: name.to_string(),
                })?
        };
        let status = session.close().await?;
        self.events.emit(SessionEvent::Closed {
            name: name.to_string(),
        });
        Ok(status)
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
            .ok_or_else(|| kmuxError::SessionNotFound {
                name: name.to_string(),
            })
    }

    /// Resize the PTY window for a named session.
    pub async fn resize(&self, name: &str, size: WindowSize) -> Result<()> {
        let sessions = self.sessions.lock().await;
        let session = sessions
            .get(name)
            .ok_or_else(|| kmuxError::SessionNotFound {
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
}
