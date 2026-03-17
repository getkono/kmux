use std::collections::HashMap;
use std::sync::Arc;

use smux::config::{PtyConfig, WindowSize};
use smux::error::{Result, SmuxError};
use smux::events::SessionEvent;
use smux::registry::SessionManager;
use smux::session::PtyWriter;
use smux_protocol::messages::{SessionInfo, TermSize};
use tokio::sync::{RwLock, broadcast};
use tracing::warn;

use crate::relay::session_read_loop;

/// Per-session relay state.
struct SessionRelay {
    /// Fan-out channel: broadcast PTY output to all attached clients.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Write half of the split session, used to forward client input.
    writer: PtyWriter,
    /// Background task that reads from the PTY and sends to `output_tx`.
    _task: tokio::task::JoinHandle<()>,
    /// Program name, stored for `SessionList` responses.
    program: String,
    /// Current terminal size.
    size: TermSize,
}

/// Shared server state — wrapped in `Arc` and cloned into each connection task.
pub struct ServerApp {
    pub manager: Arc<SessionManager>,
    pub auth_token: String,
    relays: RwLock<HashMap<String, SessionRelay>>,
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        Self {
            manager: Arc::new(SessionManager::new()),
            auth_token: token,
            relays: RwLock::new(HashMap::new()),
        }
    }

    /// Subscribe to the session lifecycle event bus.
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }

    /// Spawn a new PTY session and start its relay task.
    pub async fn create_session(
        &self,
        name: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    ) -> Result<()> {
        let prog = program.clone().unwrap_or_else(|| "/bin/bash".to_string());

        let config = PtyConfig::new(&prog).args(args).size(size.rows, size.cols);

        self.manager.spawn(name, &config).await?;

        // Split the session: the reader goes to the relay task, the writer
        // is stored in the relay struct for `write_input` calls.
        let session = self.manager.get_session(name).await?;
        let (reader, writer) = session.split();

        let (output_tx, _) = broadcast::channel(256);
        let tx = output_tx.clone();
        let task = tokio::spawn(session_read_loop(reader, tx));

        self.relays.write().await.insert(
            name.to_string(),
            SessionRelay {
                output_tx,
                writer,
                _task: task,
                program: prog,
                size,
            },
        );

        Ok(())
    }

    /// Gracefully close a session and clean up its relay.
    pub async fn close_session(&self, name: &str) -> Result<Option<i32>> {
        let status = self.manager.close(name).await?;
        self.relays.write().await.remove(name);
        Ok(status.code())
    }

    /// List all active sessions with their metadata.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let relays = self.relays.read().await;
        relays
            .iter()
            .map(|(name, relay)| SessionInfo {
                name: name.clone(),
                program: relay.program.clone(),
                size: relay.size,
            })
            .collect()
    }

    /// Subscribe to PTY output for a session.
    ///
    /// Returns a `broadcast::Receiver` that yields chunks of raw PTY output.
    pub async fn attach(&self, name: &str) -> Result<broadcast::Receiver<Vec<u8>>> {
        let relays = self.relays.read().await;
        relays
            .get(name)
            .map(|r| r.output_tx.subscribe())
            .ok_or_else(|| SmuxError::SessionNotFound {
                name: name.to_string(),
            })
    }

    /// Forward user input bytes to a named session's PTY stdin.
    pub async fn write_input(&self, name: &str, data: Vec<u8>) -> Result<()> {
        let relays = self.relays.read().await;
        let relay = relays.get(name).ok_or_else(|| SmuxError::SessionNotFound {
            name: name.to_string(),
        })?;
        relay.writer.write_all(&data).await
    }

    /// Resize a named session's PTY.
    pub async fn resize(&self, name: &str, size: TermSize) -> Result<()> {
        let ws = WindowSize {
            rows: size.rows,
            cols: size.cols,
        };
        self.manager.resize(name, ws).await?;
        // Update stored size
        if let Some(relay) = self.relays.write().await.get_mut(name) {
            relay.size = size;
        } else {
            warn!(
                "resize: relay for session '{}' not found after resize",
                name
            );
        }
        Ok(())
    }

    /// Send a Unix signal to a named session's child process.
    pub async fn send_signal(&self, name: &str, signal: i32) -> Result<()> {
        use nix::sys::signal::Signal;
        let session = self.manager.get_session(name).await?;
        let sig = Signal::try_from(signal).map_err(|_| SmuxError::Pty(nix::Error::EINVAL))?;
        session.send_signal(sig).await
    }
}
