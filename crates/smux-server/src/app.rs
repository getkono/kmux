use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use smux::config::{PtyConfig, WindowSize};
use smux::error::{Result, SmuxError};
use smux::events::SessionEvent;
use smux::registry::SessionManager;
use smux::session::PtyWriter;
use smux_protocol::messages::{SessionInfo, TermSize};
use tokio::sync::{RwLock, broadcast};
use tracing::warn;

use crate::relay::session_read_loop;
use crate::scrollback::ScrollbackBuffer;

/// 10 MB scrollback buffer per session.
const SCROLLBACK_CAPACITY: usize = 10 * 1024 * 1024;

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
    /// Ring buffer of recent PTY output, replayed to newly attaching clients.
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
}

/// Shared server state -- wrapped in `Arc` and cloned into each connection task.
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
        let prog = match program {
            Some(p) => p,
            None => smux::shell::detect_shell()?,
        };

        let config = PtyConfig::new(&prog).args(args).size(size.rows, size.cols);

        self.manager.spawn(name, &config).await?;

        // Split the session: the reader goes to the relay task, the writer
        // is stored in the relay struct for `write_input` calls.
        let session = self.manager.get_session(name).await?;
        let (reader, writer) = session.split().await?;

        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::new(SCROLLBACK_CAPACITY)));
        let (output_tx, _) = broadcast::channel(256);
        let tx = output_tx.clone();
        let task = tokio::spawn(session_read_loop(reader, tx, scrollback.clone()));

        self.relays.write().await.insert(
            name.to_string(),
            SessionRelay {
                output_tx,
                writer,
                _task: task,
                program: prog,
                size,
                scrollback,
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

    /// Subscribe to PTY output for a session and return a snapshot of buffered
    /// history for replay.
    ///
    /// The snapshot is taken while holding the `relays` read-lock, then the
    /// live receiver is subscribed. Because the relay loop writes to scrollback
    /// *before* broadcasting, any chunk that arrives after the snapshot will be
    /// delivered via the receiver -- guaranteeing no gap between history and
    /// live output.
    ///
    /// Returns `(snapshot_bytes, live_receiver)`.
    pub async fn attach(&self, name: &str) -> Result<(Vec<u8>, broadcast::Receiver<Vec<u8>>)> {
        let relays = self.relays.read().await;
        relays
            .get(name)
            .map(|r| {
                let snapshot = r.scrollback.lock().unwrap().snapshot();
                let rx = r.output_tx.subscribe();
                (snapshot, rx)
            })
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
