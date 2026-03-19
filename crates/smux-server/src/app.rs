use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::config::{PtyConfig, WindowSize};
use smux::error::{Result, SmuxError};
use smux::events::SessionEvent;
use smux::registry::SessionManager;
use smux::session::PtyWriter;
use smux_protocol::messages::{
    ClientId, InputMode, SequenceNo, ServerMessage, SessionInfo, SessionStatus, TermSize,
};
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::warn;

use crate::relay::session_read_loop;
use crate::scrollback::SeqnoBuffer;

/// 10 MB scrollback buffer per session.
const SCROLLBACK_CAPACITY: usize = 10 * 1024 * 1024;

/// Shared map of per-client output senders for a single session.
pub type ClientMap = Arc<Mutex<HashMap<ClientId, mpsc::Sender<ServerMessage>>>>;

/// Per-session relay state.
pub struct SessionRelay {
    /// Per-client output senders, shared with the relay task.
    pub clients: ClientMap,
    /// Write half of the split session, used to forward client input.
    pub writer: PtyWriter,
    /// Background task that reads from the PTY and sends to each client.
    pub _task: tokio::task::JoinHandle<()>,
    /// Program name, stored for `SessionList` responses.
    pub program: String,
    /// Current terminal size.
    pub size: TermSize,
    /// Ring buffer of recent PTY output, keyed by sequence number.
    pub scrollback: Arc<Mutex<SeqnoBuffer>>,
    /// Input control mode for this session.
    pub input_mode: InputMode,
    /// Session lifecycle status (Running or Exited).
    pub status: SessionStatus,
}

/// Shared server state — wrapped in `Arc` and cloned into each connection task.
pub struct ServerApp {
    pub manager: Arc<SessionManager>,
    pub auth_token: String,
    relays: RwLock<HashMap<String, SessionRelay>>,
    /// Monotonic client ID counter.
    next_client_id: AtomicU64,
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        Self {
            manager: Arc::new(SessionManager::new()),
            auth_token: token,
            relays: RwLock::new(HashMap::new()),
            next_client_id: AtomicU64::new(1),
        }
    }

    /// Assign a fresh monotonic `ClientId`.
    pub fn next_client_id(&self) -> ClientId {
        ClientId(self.next_client_id.fetch_add(1, Ordering::Relaxed))
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

        let session = self.manager.get_session(name).await?;
        let (reader, writer) = session.split().await?;

        // Shared state between SessionRelay and relay task
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(SeqnoBuffer::new(SCROLLBACK_CAPACITY)));

        let task = tokio::spawn(session_read_loop(
            reader,
            name.to_string(),
            clients.clone(),
            scrollback.clone(),
        ));

        self.relays.write().await.insert(
            name.to_string(),
            SessionRelay {
                clients,
                writer,
                _task: task,
                program: prog,
                size,
                scrollback,
                input_mode: InputMode::Open,
                status: SessionStatus::Running,
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
            .map(|(name, relay)| {
                let attached_clients = relay.clients.lock().unwrap().keys().copied().collect();
                SessionInfo {
                    name: name.clone(),
                    program: relay.program.clone(),
                    size: relay.size,
                    attached_clients,
                    status: relay.status.clone(),
                }
            })
            .collect()
    }

    /// Register a client's output channel for a session and return replay data.
    ///
    /// Returns an `AttachResult` describing what scrollback to send, plus
    /// inserts `client_tx` into the session's client map so the relay task
    /// starts forwarding live output.
    pub async fn attach(
        &self,
        name: &str,
        client_id: ClientId,
        last_seqno: Option<SequenceNo>,
        client_tx: mpsc::Sender<ServerMessage>,
    ) -> Result<AttachResult> {
        let relays = self.relays.read().await;
        let relay = relays.get(name).ok_or_else(|| SmuxError::SessionNotFound {
            name: name.to_string(),
        })?;

        let result = {
            let buf = relay.scrollback.lock().unwrap();
            match last_seqno {
                None => AttachResult::FullSnapshot(buf.snapshot()),
                Some(seq) => match buf.oldest_seqno() {
                    Some(oldest) if seq >= oldest => AttachResult::Delta(buf.since(seq)),
                    _ => AttachResult::SyncReset(buf.snapshot()),
                },
            }
        };

        // Register client — relay task will now deliver live output to them.
        relay.clients.lock().unwrap().insert(client_id, client_tx);

        Ok(result)
    }

    /// Remove a client from a specific session and release any lock they hold.
    pub async fn detach_from_session(&self, name: &str, client_id: ClientId) {
        let mut relays = self.relays.write().await;
        if let Some(relay) = relays.get_mut(name) {
            relay.clients.lock().unwrap().remove(&client_id);
            if relay.input_mode == InputMode::Locked(client_id) {
                relay.input_mode = InputMode::Open;
            }
        }
    }

    /// Remove a client from all sessions they were attached to.
    pub async fn detach_client_all(&self, client_id: ClientId) {
        let mut relays = self.relays.write().await;
        for relay in relays.values_mut() {
            relay.clients.lock().unwrap().remove(&client_id);
            if relay.input_mode == InputMode::Locked(client_id) {
                relay.input_mode = InputMode::Open;
            }
        }
    }

    /// Forward user input bytes to a named session's PTY stdin.
    /// Enforces the session's `InputMode` — returns `Err(EPERM)` if denied.
    pub async fn write_input(&self, name: &str, client_id: ClientId, data: Vec<u8>) -> Result<()> {
        let relays = self.relays.read().await;
        let relay = relays.get(name).ok_or_else(|| SmuxError::SessionNotFound {
            name: name.to_string(),
        })?;

        match &relay.input_mode {
            InputMode::Open => {}
            InputMode::Locked(holder) if *holder == client_id => {}
            InputMode::Locked(_) | InputMode::Disabled => {
                return Err(SmuxError::Pty(nix::Error::EPERM));
            }
        }

        relay.writer.write_all(&data).await
    }

    /// Resize a named session's PTY.
    pub async fn resize(&self, name: &str, size: TermSize) -> Result<()> {
        let ws = WindowSize {
            rows: size.rows,
            cols: size.cols,
        };
        self.manager.resize(name, ws).await?;
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

    /// Request an exclusive input lock for `client_id` on `session`.
    pub async fn request_input_lock(
        &self,
        name: &str,
        client_id: ClientId,
    ) -> Result<InputLockOutcome> {
        let mut relays = self.relays.write().await;
        let relay = relays
            .get_mut(name)
            .ok_or_else(|| SmuxError::SessionNotFound {
                name: name.to_string(),
            })?;
        match &relay.input_mode {
            InputMode::Open => {
                relay.input_mode = InputMode::Locked(client_id);
                Ok(InputLockOutcome::Granted)
            }
            InputMode::Locked(holder) if *holder == client_id => {
                Ok(InputLockOutcome::Granted) // already held — idempotent
            }
            InputMode::Locked(holder) => Ok(InputLockOutcome::Denied(*holder)),
            InputMode::Disabled => Ok(InputLockOutcome::Denied(ClientId(0))),
        }
    }

    /// Release the input lock held by `client_id` on `session`.
    /// Returns `true` if the lock was actually released.
    pub async fn release_input_lock(&self, name: &str, client_id: ClientId) -> Result<bool> {
        let mut relays = self.relays.write().await;
        let relay = relays
            .get_mut(name)
            .ok_or_else(|| SmuxError::SessionNotFound {
                name: name.to_string(),
            })?;
        if relay.input_mode == InputMode::Locked(client_id) {
            relay.input_mode = InputMode::Open;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Rename a session. Returns `Err` if the new name already exists.
    pub async fn rename_session(&self, old_name: &str, new_name: &str) -> Result<()> {
        let mut relays = self.relays.write().await;
        if relays.contains_key(new_name) {
            return Err(SmuxError::SessionAlreadyExists {
                name: new_name.to_string(),
            });
        }
        let relay = relays
            .remove(old_name)
            .ok_or_else(|| SmuxError::SessionNotFound {
                name: old_name.to_string(),
            })?;
        relays.insert(new_name.to_string(), relay);
        Ok(())
    }

    /// Mark a session as exited (called from event bus listener).
    #[allow(dead_code)]
    pub async fn mark_session_exited(&self, name: &str, code: Option<i32>, signal: Option<i32>) {
        let mut relays = self.relays.write().await;
        if let Some(relay) = relays.get_mut(name) {
            relay.status = SessionStatus::Exited { code, signal };
        }
    }
}

/// Outcome of an input lock request.
pub enum InputLockOutcome {
    Granted,
    Denied(ClientId),
}

/// Result of an attach operation describing what replay data to send.
pub enum AttachResult {
    /// Fresh attach or first-time connect: all scrollback bytes concatenated.
    FullSnapshot(Vec<u8>),
    /// Delta replay: only chunks with seqno > last_seqno.
    Delta(Vec<(SequenceNo, Vec<u8>)>),
    /// Requested seqno was too old; full snapshot sent, client must reset state.
    SyncReset(Vec<u8>),
}
