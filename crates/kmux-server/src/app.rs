use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{
    ClientId, GridSnapshot, InputMode, PaneId, PaneInfo, SequenceNo, ServerMessage, SessionEntry,
    SessionMeta, SessionStatus, TermSize, TerminalDiff, WordId,
};
use kmux_pty::config::{PtyConfig, WindowSize};
use kmux_pty::error::{KmuxError, Result};
use kmux_pty::events::SessionEvent;
use kmux_pty::registry::SessionManager as PtyRegistry;
use kmux_pty::session::PtyWriter;
use rand::SeedableRng;
use tokio::sync::{RwLock, broadcast, mpsc};
use tracing::warn;

use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::{TermState, new_term_state};
use crate::wordlist::WordlistSampler;

/// 10 MB scrollback buffer per pane (estimated diff size).
const SCROLLBACK_CAPACITY: usize = 10 * 1024 * 1024;

/// Maximum number of active sessions per daemon.
const MAX_SESSIONS: usize = 1000;

/// Per-client sender pair: bounded data channel (for diffs) + unbounded control
/// channel (for notifications like `Lagged` that must never be dropped).
pub struct ClientSender {
    pub data_tx: mpsc::Sender<ServerMessage>,
    pub ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    /// When true, the relay sends full `TerminalSnapshot` messages instead
    /// of incremental `TerminalUpdate` diffs.
    pub force_full_snapshot: bool,
}

/// Shared map of per-client output senders for a single pane.
pub type ClientMap = Arc<Mutex<HashMap<ClientId, ClientSender>>>;

/// Per-pane relay state (formerly `SessionRelay`).
pub struct PaneRelay {
    /// Per-client output senders, shared with the relay task.
    pub clients: ClientMap,
    /// Write half of the split pane, used to forward client input.
    pub writer: PtyWriter,
    /// Background task that reads from the PTY and sends to each client.
    pub _task: tokio::task::JoinHandle<()>,
    /// Program name, stored for `SessionList` responses.
    pub program: String,
    /// Current terminal size.
    pub size: TermSize,
    /// Ring buffer of recent diffs, keyed by sequence number.
    pub scrollback: Arc<Mutex<DiffBuffer>>,
    /// Server-side VT emulation state for this pane.
    pub term_state: Arc<Mutex<TermState>>,
    /// Monotonic seqno counter shared with the relay diff loop.
    pub seqno_counter: Arc<AtomicU64>,
    /// Input control mode for this pane.
    pub input_mode: InputMode,
    /// Pane lifecycle status (Running or Exited).
    pub status: SessionStatus,
}

/// State for one session: its metadata plus all its panes.
pub struct SessionState {
    pub meta: SessionMeta,
    /// Map of pane_index -> PaneRelay.
    pub panes: HashMap<u32, PaneRelay>,
    /// Next pane index to assign (monotonically increasing within this session).
    pub next_pane_index: u32,
}

/// Shared server state -- wrapped in `Arc` and cloned into each connection task.
pub struct ServerApp {
    /// PTY registry for spawning and managing child processes.
    pub manager: Arc<PtyRegistry>,
    pub auth_token: String,
    /// Map of word_id -> SessionState.
    sessions: RwLock<HashMap<WordId, SessionState>>,
    /// Monotonic session creation counter.
    session_index_counter: AtomicU32,
    /// Monotonic client ID counter.
    next_client_id: AtomicU64,
    /// Word pool for assigning unique session IDs.
    wordlist: Mutex<WordlistSampler>,
    /// RNG for word selection (seeded once at startup).
    rng: Mutex<rand::rngs::SmallRng>,
}

impl ServerApp {
    pub fn new(token: String) -> Self {
        Self {
            manager: Arc::new(PtyRegistry::new()),
            auth_token: token,
            sessions: RwLock::new(HashMap::new()),
            session_index_counter: AtomicU32::new(0),
            next_client_id: AtomicU64::new(1),
            wordlist: Mutex::new(WordlistSampler::new()),
            rng: Mutex::new(rand::rngs::SmallRng::from_os_rng()),
        }
    }

    /// Assign a fresh monotonic `ClientId`.
    pub fn next_client_id(&self) -> ClientId {
        ClientId(self.next_client_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Subscribe to the PTY lifecycle event bus.
    pub fn subscribe_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.manager.subscribe()
    }

    // ── Session/Pane creation ─────────────────────────────────────────────────

    /// Create a new session with one initial pane. Returns the full `SessionEntry`.
    pub async fn create_session(
        &self,
        name: Option<String>,
        cwd: Option<String>,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    ) -> Result<SessionEntry> {
        // Check the session limit
        {
            let sessions = self.sessions.read().await;
            if sessions.len() >= MAX_SESSIONS {
                return Err(KmuxError::SessionAlreadyExists {
                    name: format!("session limit ({MAX_SESSIONS}) reached"),
                });
            }
        }

        // Draw a unique word ID
        let word_id = {
            let mut wl = self.wordlist.lock().unwrap();
            let mut rng = self.rng.lock().unwrap();
            wl.draw(&mut *rng)
                .ok_or_else(|| KmuxError::SessionAlreadyExists {
                    name: "word pool exhausted".to_string(),
                })?
        };

        // Resolve CWD
        let resolved_cwd = resolve_cwd(
            cwd.as_deref()
                .map(Path::new)
                .unwrap_or_else(|| Path::new(".")),
        );

        // Default name = basename of cwd
        let display_name = name.unwrap_or_else(|| {
            resolved_cwd
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&word_id)
                .to_string()
        });

        let index = self.session_index_counter.fetch_add(1, Ordering::Relaxed);

        let meta = SessionMeta {
            index,
            word_id: word_id.clone(),
            name: display_name,
            cwd: resolved_cwd.to_string_lossy().into_owned(),
        };

        // Spawn initial pane (index 0)
        let pane_index = 0u32;
        let pane_id = format!("{word_id}/{pane_index}");
        let relay = self
            .spawn_pane_relay(&pane_id, program, args, size, Some(&resolved_cwd))
            .await?;

        let pane_info = PaneInfo {
            pane_id: pane_id.clone(),
            pane_index,
            program: relay.program.clone(),
            size: relay.size,
            attached_clients: vec![],
            status: SessionStatus::Running,
        };

        let mut panes = HashMap::new();
        panes.insert(pane_index, relay);

        let state = SessionState {
            meta: meta.clone(),
            panes,
            next_pane_index: 1,
        };

        self.sessions.write().await.insert(word_id.clone(), state);

        Ok(SessionEntry {
            meta,
            panes: vec![pane_info],
        })
    }

    /// Add a new pane to an existing session.
    pub async fn create_pane(
        &self,
        word_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
    ) -> Result<PaneId> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;

        let pane_index = state.next_pane_index;
        state.next_pane_index += 1;
        let pane_id = format!("{word_id}/{pane_index}");

        let cwd = PathBuf::from(&state.meta.cwd);
        let effective_cwd = resolve_cwd(&cwd);

        // Drop the write lock before spawning (IO)
        let prog = match program {
            Some(ref p) => p.clone(),
            None => kmux_pty::shell::detect_shell()?,
        };
        let resolved_cwd_clone = effective_cwd.clone();
        drop(sessions);

        let config = PtyConfig::new(&prog)
            .args(args.clone())
            .size(size.rows, size.cols)
            .cwd(resolved_cwd_clone);
        self.manager.spawn(&pane_id, &config).await?;

        let session = self.manager.get_session(&pane_id).await?;
        let (reader, writer) = session.split().await?;

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
        let term_state = Arc::new(Mutex::new(new_term_state(size.rows, size.cols)));
        let seqno_counter = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.clone(),
            clients.clone(),
            scrollback.clone(),
            term_state.clone(),
            seqno_counter.clone(),
        ));

        let relay = PaneRelay {
            clients,
            writer,
            _task: task,
            program: prog,
            size,
            scrollback,
            term_state,
            seqno_counter,
            input_mode: InputMode::Open,
            status: SessionStatus::Running,
        };

        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        state.panes.insert(pane_index, relay);

        Ok(pane_id)
    }

    // ── Session/Pane close ────────────────────────────────────────────────────

    /// Gracefully close all panes of a session and remove it.
    pub async fn close_session(&self, word_id: &str) -> Result<Option<i32>> {
        let pane_ids: Vec<(u32, String)> = {
            let sessions = self.sessions.read().await;
            sessions
                .get(word_id)
                .map(|s| {
                    s.panes
                        .keys()
                        .map(|&idx| (idx, format!("{word_id}/{idx}")))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut last_exit = None;
        for (_, pane_id) in &pane_ids {
            if let Ok(code) = self.manager.close(pane_id).await {
                last_exit = code.code();
            }
        }

        // Remove session state
        let mut sessions = self.sessions.write().await;
        sessions.remove(word_id);

        // Return word to pool
        self.wordlist.lock().unwrap().release(word_id);

        Ok(last_exit)
    }

    /// Gracefully close a single pane.
    /// If it was the last pane in its session, also removes the session.
    pub async fn close_pane(&self, pane_id: &str) -> Result<Option<i32>> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        // Detach all clients from this pane
        {
            let sessions = self.sessions.read().await;
            if let Some(state) = sessions.get(word_id)
                && let Some(relay) = state.panes.get(&pane_index)
            {
                relay.clients.lock().unwrap().clear();
            }
        }

        let status = self.manager.close(pane_id).await?;
        let exit_code = status.code();

        // Remove pane from session; remove session if empty
        let mut sessions = self.sessions.write().await;
        if let Some(state) = sessions.get_mut(word_id) {
            state.panes.remove(&pane_index);
            if state.panes.is_empty() {
                let word = word_id.to_string();
                sessions.remove(&word);
                self.wordlist.lock().unwrap().release(&word);
            }
        }

        Ok(exit_code)
    }

    // ── Session listing ───────────────────────────────────────────────────────

    /// List all active sessions with their pane metadata.
    pub async fn list_sessions(&self) -> Vec<SessionEntry> {
        let sessions = self.sessions.read().await;
        let mut entries: Vec<SessionEntry> = sessions
            .values()
            .map(|state| {
                let mut panes: Vec<PaneInfo> = state
                    .panes
                    .iter()
                    .map(|(&pane_index, relay)| {
                        let attached_clients =
                            relay.clients.lock().unwrap().keys().copied().collect();
                        PaneInfo {
                            pane_id: format!("{}/{pane_index}", state.meta.word_id),
                            pane_index,
                            program: relay.program.clone(),
                            size: relay.size,
                            attached_clients,
                            status: relay.status.clone(),
                        }
                    })
                    .collect();
                panes.sort_by_key(|p| p.pane_index);
                SessionEntry {
                    meta: state.meta.clone(),
                    panes,
                }
            })
            .collect();
        entries.sort_by_key(|e| e.meta.index);
        entries
    }

    // ── Rename ────────────────────────────────────────────────────────────────

    /// Rename a session's display name.
    pub async fn rename_session(&self, word_id: &str, new_name: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: word_id.to_string(),
            })?;
        state.meta.name = new_name.to_string();
        Ok(())
    }

    // ── Attach/Detach ─────────────────────────────────────────────────────────

    /// Register a client's output channel for a pane and return replay data.
    pub async fn attach(
        &self,
        pane_id: &str,
        client_id: ClientId,
        last_seqno: Option<SequenceNo>,
        data_tx: mpsc::Sender<ServerMessage>,
        ctrl_tx: mpsc::UnboundedSender<ServerMessage>,
    ) -> Result<AttachResult> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        let sessions = self.sessions.read().await;
        let state = sessions
            .get(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;

        let result = match last_seqno {
            None => {
                let snapshot = relay.term_state.lock().unwrap().snapshot();
                let current_seqno = SequenceNo(
                    relay
                        .seqno_counter
                        .load(Ordering::Relaxed)
                        .saturating_sub(1),
                );
                AttachResult::FullSnapshot(snapshot, current_seqno)
            }
            Some(seq) => {
                let buf = relay.scrollback.lock().unwrap();
                match buf.oldest_seqno() {
                    Some(oldest) if seq >= oldest => AttachResult::Delta(buf.since(seq)),
                    _ => {
                        let snapshot = relay.term_state.lock().unwrap().snapshot();
                        let current_seqno = SequenceNo(
                            relay
                                .seqno_counter
                                .load(Ordering::Relaxed)
                                .saturating_sub(1),
                        );
                        AttachResult::SyncReset(snapshot, current_seqno)
                    }
                }
            }
        };

        relay.clients.lock().unwrap().insert(
            client_id,
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
            },
        );

        Ok(result)
    }

    /// Set the full-snapshot mode flag for a client across all attached panes.
    pub async fn set_snapshot_mode(&self, client_id: ClientId, enabled: bool) {
        let sessions = self.sessions.read().await;
        for state in sessions.values() {
            for relay in state.panes.values() {
                let mut map = relay.clients.lock().unwrap();
                if let Some(sender) = map.get_mut(&client_id) {
                    sender.force_full_snapshot = enabled;
                }
            }
        }
    }

    /// Remove a client from a specific pane and release any lock they hold.
    pub async fn detach_from_pane(&self, pane_id: &str, client_id: ClientId) {
        let Some((word_id, pane_index)) = parse_pane_id(pane_id) else {
            return;
        };
        let mut sessions = self.sessions.write().await;
        if let Some(state) = sessions.get_mut(word_id)
            && let Some(relay) = state.panes.get_mut(&pane_index)
        {
            relay.clients.lock().unwrap().remove(&client_id);
            if relay.input_mode == InputMode::Locked(client_id) {
                relay.input_mode = InputMode::Open;
            }
        }
    }

    /// Remove a client from all panes they were attached to.
    pub async fn detach_client_all(&self, client_id: ClientId) {
        let mut sessions = self.sessions.write().await;
        for state in sessions.values_mut() {
            for relay in state.panes.values_mut() {
                relay.clients.lock().unwrap().remove(&client_id);
                if relay.input_mode == InputMode::Locked(client_id) {
                    relay.input_mode = InputMode::Open;
                }
            }
        }
    }

    // ── PTY I/O ───────────────────────────────────────────────────────────────

    /// Forward user input bytes to a pane's PTY stdin.
    pub async fn write_input(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data: Vec<u8>,
    ) -> Result<()> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let sessions = self.sessions.read().await;
        let state = sessions
            .get(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        match &relay.input_mode {
            InputMode::Open => {}
            InputMode::Locked(holder) if *holder == client_id => {}
            InputMode::Locked(_) | InputMode::Disabled => {
                return Err(KmuxError::Pty(nix::Error::EPERM));
            }
        }
        relay.writer.write_all(&data).await
    }

    /// Paste clipboard text into a pane's PTY stdin.
    pub async fn write_paste(
        &self,
        pane_id: &str,
        client_id: ClientId,
        data: String,
    ) -> Result<()> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let sessions = self.sessions.read().await;
        let state = sessions
            .get(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        match &relay.input_mode {
            InputMode::Open => {}
            InputMode::Locked(holder) if *holder == client_id => {}
            InputMode::Locked(_) | InputMode::Disabled => {
                return Err(KmuxError::Pty(nix::Error::EPERM));
            }
        }
        let bracketed = relay.term_state.lock().unwrap().modes().bracketed_paste();
        if bracketed {
            let mut buf = Vec::with_capacity(data.len() + 12);
            buf.extend_from_slice(b"\x1b[200~");
            buf.extend_from_slice(data.as_bytes());
            buf.extend_from_slice(b"\x1b[201~");
            relay.writer.write_all(&buf).await
        } else {
            relay.writer.write_all(data.as_bytes()).await
        }
    }

    /// Resize a pane's PTY and its server-side terminal emulator.
    pub async fn resize(&self, pane_id: &str, size: TermSize) -> Result<()> {
        let ws = WindowSize {
            rows: size.rows,
            cols: size.cols,
        };
        self.manager.resize(pane_id, ws).await?;

        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let mut sessions = self.sessions.write().await;
        if let Some(state) = sessions.get_mut(word_id) {
            if let Some(relay) = state.panes.get_mut(&pane_index) {
                relay.size = size;
                relay
                    .term_state
                    .lock()
                    .unwrap()
                    .resize(size.rows, size.cols);
            } else {
                warn!("resize: pane '{pane_id}' not found after resize");
            }
        }
        Ok(())
    }

    /// Send a Unix signal to a pane's child process.
    pub async fn send_signal(&self, pane_id: &str, signal: i32) -> Result<()> {
        use nix::sys::signal::Signal;
        let session = self.manager.get_session(pane_id).await?;
        let sig = Signal::try_from(signal).map_err(|_| KmuxError::Pty(nix::Error::EINVAL))?;
        session.send_signal(sig).await
    }

    // ── Input lock ────────────────────────────────────────────────────────────

    /// Request an exclusive input lock for `client_id` on `pane_id`.
    pub async fn request_input_lock(
        &self,
        pane_id: &str,
        client_id: ClientId,
    ) -> Result<InputLockOutcome> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get_mut(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        match &relay.input_mode {
            InputMode::Open => {
                relay.input_mode = InputMode::Locked(client_id);
                Ok(InputLockOutcome::Granted)
            }
            InputMode::Locked(holder) if *holder == client_id => {
                Ok(InputLockOutcome::Granted) // idempotent
            }
            InputMode::Locked(holder) => Ok(InputLockOutcome::Denied(*holder)),
            InputMode::Disabled => Ok(InputLockOutcome::Denied(ClientId(0))),
        }
    }

    /// Release the input lock held by `client_id` on `pane_id`.
    pub async fn release_input_lock(&self, pane_id: &str, client_id: ClientId) -> Result<bool> {
        let (word_id, pane_index) =
            parse_pane_id(pane_id).ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let mut sessions = self.sessions.write().await;
        let state = sessions
            .get_mut(word_id)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        let relay = state
            .panes
            .get_mut(&pane_index)
            .ok_or_else(|| KmuxError::SessionNotFound {
                name: pane_id.to_string(),
            })?;
        if relay.input_mode == InputMode::Locked(client_id) {
            relay.input_mode = InputMode::Open;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    /// Spawn a PTY process and create a `PaneRelay` for it.
    async fn spawn_pane_relay(
        &self,
        pane_id: &str,
        program: Option<String>,
        args: Vec<String>,
        size: TermSize,
        cwd: Option<&Path>,
    ) -> Result<PaneRelay> {
        let prog = match program {
            Some(p) => p,
            None => kmux_pty::shell::detect_shell()?,
        };

        let mut config = PtyConfig::new(&prog).args(args).size(size.rows, size.cols);
        if let Some(cwd_path) = cwd {
            config = config.cwd(cwd_path);
        }

        self.manager.spawn(pane_id, &config).await?;
        let session = self.manager.get_session(pane_id).await?;
        let (reader, writer) = session.split().await?;

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
        let term_state = Arc::new(Mutex::new(new_term_state(size.rows, size.cols)));
        let seqno_counter = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(session_diff_loop(
            reader,
            pane_id.to_string(),
            clients.clone(),
            scrollback.clone(),
            term_state.clone(),
            seqno_counter.clone(),
        ));

        Ok(PaneRelay {
            clients,
            writer,
            _task: task,
            program: prog,
            size,
            scrollback,
            term_state,
            seqno_counter,
            input_mode: InputMode::Open,
            status: SessionStatus::Running,
        })
    }
}

/// Parse a pane ID `"{word_id}/{pane_index}"` into its components.
pub fn parse_pane_id(pane_id: &str) -> Option<(&str, u32)> {
    let (word, idx_str) = pane_id.rsplit_once('/')?;
    let idx: u32 = idx_str.parse().ok()?;
    Some((word, idx))
}

/// Walk up the directory tree to find the nearest existing ancestor.
fn resolve_cwd(desired: &Path) -> PathBuf {
    let mut p = desired.to_path_buf();
    loop {
        if p.exists() {
            return p;
        }
        if !p.pop() {
            return home_dir();
        }
    }
}

/// Return the user's home directory, falling back to `/`.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Outcome of an input lock request.
pub enum InputLockOutcome {
    Granted,
    Denied(ClientId),
}

/// Result of an attach operation describing what replay data to send.
pub enum AttachResult {
    /// Fresh attach or first-time connect: full grid snapshot from TermState.
    FullSnapshot(GridSnapshot, SequenceNo),
    /// Delta replay: only diffs with seqno > last_seqno.
    Delta(Vec<(SequenceNo, Arc<TerminalDiff>)>),
    /// Requested seqno was too old; full snapshot sent, client must reset state.
    SyncReset(GridSnapshot, SequenceNo),
}
