use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kmux_protocol::messages::{
    CellAttrs, CellState, GridSnapshot, InputMode, SequenceNo, SessionStatus,
};
use kmux_pty::config::{EnvBuilder, PtyConfig};
use tracing::warn;

use crate::diff_engine::DiffResult;
use crate::relay::session_diff_loop;
use crate::scrollback::DiffBuffer;
use crate::term_state::{TermState, new_term_state};

use super::helpers::resolve_cwd;
use super::{ClientMap, PaneRelay, SCROLLBACK_CAPACITY, ServerApp, SessionState};

/// Summary of a session restore operation.
#[derive(Debug, Default)]
pub struct RestoreReport {
    /// Total sessions attempted.
    pub restored: usize,
    /// Sessions whose child process was still alive and reattached.
    pub alive: usize,
    /// Sessions whose child process had exited (scrollback preserved).
    pub dead: usize,
}

impl ServerApp {
    /// Capture the current daemon state for checkpointing.
    ///
    /// Called by the persist layer to snapshot all sessions/panes without
    /// requiring it to access private fields directly.
    pub async fn checkpoint_state(&self) -> crate::persist::PersistedDaemonState {
        use crate::persist::{
            MAX_SCROLLBACK_LINES, PersistedDaemonState, PersistedPane, PersistedSession,
            STATE_VERSION,
        };

        let sessions_guard = self.sessions.read().await;
        let session_index_counter = self.session_index_counter.load(Ordering::Relaxed);
        let used_words: Vec<String> = sessions_guard.keys().cloned().collect();
        let mut persisted_sessions = Vec::with_capacity(sessions_guard.len());

        for (word_id, session_state) in sessions_guard.iter() {
            let mut persisted_panes = Vec::with_capacity(session_state.panes.len());

            for (&pane_index, relay) in session_state.panes.iter() {
                let pane_id = format!("{word_id}/{pane_index}");

                // Snapshot grid state (backend-agnostic via DiffEngine).
                let grid = relay.term_state.lock().unwrap().snapshot();

                // Extract scrollback from the backend.
                let scrollback_lines = {
                    let ts = relay.term_state.lock().unwrap();
                    let size = ts.history_size();
                    let start = size.saturating_sub(MAX_SCROLLBACK_LINES);
                    let count = size - start;
                    if count > 0 {
                        ts.read_history_lines(start, count)
                    } else {
                        vec![]
                    }
                };

                // Get child PID from the PTY registry.
                let child_pid = self.manager.child_pid(&pane_id).await.map(|p| p.as_raw());

                persisted_panes.push(PersistedPane {
                    pane_index,
                    program: relay.program.clone(),
                    args: relay.args.clone(),
                    size: relay.size,
                    status: relay.status.clone(),
                    child_pid,
                    grid,
                    scrollback_lines,
                    cwd: session_state.meta.cwd.clone(),
                });
            }

            persisted_panes.sort_by_key(|p| p.pane_index);

            persisted_sessions.push(PersistedSession {
                meta: session_state.meta.clone(),
                next_pane_index: session_state.next_pane_index,
                panes: persisted_panes,
            });
        }

        persisted_sessions.sort_by_key(|s| s.meta.index);

        PersistedDaemonState {
            version: STATE_VERSION,
            session_index_counter,
            sessions: persisted_sessions,
            used_words,
        }
    }

    /// Restore sessions from a [`PersistedDaemonState`].
    ///
    /// For each persisted pane, spawns a fresh shell using the same program and
    /// args as the original.  Before the new shell outputs its prompt, the old
    /// terminal grid is replayed as ANSI bytes so the client sees the previous
    /// visual state above a "session restored" separator line.
    ///
    /// Returns a [`RestoreReport`] suitable for logging.
    pub async fn restore_from(&self, state: crate::persist::PersistedDaemonState) -> RestoreReport {
        let mut report = RestoreReport::default();

        // Restore the session_index_counter to at least the checkpoint value.
        let _ = self
            .session_index_counter
            .fetch_max(state.session_index_counter, Ordering::Relaxed);

        // Reserve word IDs so the wordlist doesn't re-issue them.
        {
            let mut wl = self.wordlist.lock().unwrap();
            for word in &state.used_words {
                wl.reserve(word);
            }
        }

        for persisted_session in state.sessions {
            let word_id = persisted_session.meta.word_id.clone();
            let mut panes_map: HashMap<u32, PaneRelay> = HashMap::new();
            let session_cwd = PathBuf::from(&persisted_session.meta.cwd);
            let effective_cwd = resolve_cwd(&session_cwd);

            for persisted_pane in persisted_session.panes {
                let pane_index = persisted_pane.pane_index;
                let pane_id = format!("{word_id}/{pane_index}");
                let size = persisted_pane.size;

                // Spawn a fresh shell using the persisted program and args.
                let config = PtyConfig::new(&persisted_pane.program)
                    .args(persisted_pane.args.clone())
                    .size(size.rows, size.cols)
                    .cwd(&effective_cwd)
                    .env(EnvBuilder::new().auto_term(false));

                if let Err(e) = self.manager.spawn(&pane_id, &config).await {
                    warn!("restore: failed to spawn fresh shell for {pane_id}: {e}");
                    continue;
                }

                let session = match self.manager.get_session(&pane_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("restore: could not get session {pane_id}: {e}");
                        continue;
                    }
                };

                let (reader, writer) = match session.split().await {
                    Ok(rw) => rw,
                    Err(e) => {
                        warn!("restore: could not split session {pane_id}: {e}");
                        continue;
                    }
                };

                let kitty_graphics_enabled = Arc::new(AtomicBool::new(false));
                let kitty_keyboard_enabled = Arc::new(AtomicBool::new(false));
                let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
                let scrollback = Arc::new(Mutex::new(DiffBuffer::new(SCROLLBACK_CAPACITY)));
                let term_state = Arc::new(Mutex::new(new_term_state(
                    size.rows,
                    size.cols,
                    kitty_graphics_enabled.clone(),
                    kitty_keyboard_enabled.clone(),
                )));
                let seqno_counter = Arc::new(AtomicU64::new(1));

                // Pre-feed the old scrollback history + visible grid as ANSI
                // bytes so the client can scroll up through the full history.
                // Done synchronously before spawning the diff loop so that
                // `snapshot()` already has the restored content when a client
                // attaches immediately after daemon restart.
                let preamble =
                    snapshot_to_ansi(&persisted_pane.grid, &persisted_pane.scrollback_lines);
                seed_pane_with_preamble(&term_state, &scrollback, &seqno_counter, &preamble);

                let task = tokio::spawn(session_diff_loop(
                    reader,
                    pane_id.clone(),
                    clients.clone(),
                    scrollback.clone(),
                    term_state.clone(),
                    seqno_counter.clone(),
                ));

                panes_map.insert(
                    pane_index,
                    PaneRelay {
                        clients,
                        writer,
                        _task: task,
                        program: persisted_pane.program.clone(),
                        args: persisted_pane.args.clone(),
                        size,
                        scrollback,
                        term_state,
                        seqno_counter,
                        input_mode: InputMode::Open,
                        status: SessionStatus::Running,
                        kitty_graphics_enabled,
                        kitty_keyboard_enabled,
                    },
                );
                report.restored += 1;
            }

            if panes_map.is_empty() {
                // All panes failed to spawn — release the word ID.
                self.wordlist
                    .lock()
                    .unwrap()
                    .release(&persisted_session.meta.word_id);
                continue;
            }

            let session_state = SessionState {
                meta: persisted_session.meta.clone(),
                panes: panes_map,
                next_pane_index: persisted_session.next_pane_index,
            };

            self.sessions
                .write()
                .await
                .insert(word_id.clone(), session_state);
        }

        report
    }
}

/// Convert a [`GridSnapshot`] plus scrollback history to ANSI/VT100 escape
/// sequences that reproduce the full terminal history when fed to a fresh
/// terminal emulator.
///
/// Emits the scrollback lines first (oldest → newest), then the visible
/// viewport rows, then a dim "session restored" separator.  Because a real
/// terminal emulator scrolls lines into its history buffer as content flows
/// past the top of the screen, the user can scroll up to see the entire
/// restored history.
fn snapshot_to_ansi(snapshot: &GridSnapshot, scrollback_lines: &[Vec<CellState>]) -> Vec<u8> {
    let mut out = Vec::new();

    // Reset all SGR attributes before rendering.
    out.extend_from_slice(b"\x1b[0m");

    // Emit scrollback history first so it scrolls into the backend's history
    // buffer.  Each line ends with \r\n which advances the terminal row.
    for line in scrollback_lines {
        emit_cells_line(&mut out, line);
    }

    // Emit the visible viewport rows.
    let rows = snapshot.rows as usize;
    let cols = snapshot.cols as usize;
    for row in 0..rows {
        let base = row * cols;
        if base + cols > snapshot.cells.len() {
            break;
        }
        emit_cells_line(&mut out, &snapshot.cells[base..base + cols]);
    }

    // Dim separator visually distinguishing the restored history from the new shell.
    out.extend_from_slice(b"\x1b[2m[kmux: session restored]\x1b[0m\r\n");

    out
}

/// Emit one row of cells as ANSI bytes into `out`, followed by `\r\n`.
///
/// Trailing spaces are trimmed.  SGR sequences are coalesced so that only one
/// escape is emitted per style-change boundary.
fn emit_cells_line(out: &mut Vec<u8>, cells: &[CellState]) {
    use std::fmt::Write as FmtWrite;

    let last_content = cells
        .iter()
        .rposition(|cell| cell.c != ' ')
        .map(|i| i + 1)
        .unwrap_or(0);

    if last_content > 0 {
        let mut prev_key: Option<(u8, u8, u8, u8, u8, u8, u16)> = None;

        for cell in &cells[..last_content] {
            let style_key = (
                cell.fg.r,
                cell.fg.g,
                cell.fg.b,
                cell.bg.r,
                cell.bg.g,
                cell.bg.b,
                cell.attrs.0,
            );
            if prev_key != Some(style_key) {
                prev_key = Some(style_key);
                let mut sgr = String::from("\x1b[0");
                if cell.attrs.contains(CellAttrs::BOLD) {
                    sgr.push_str(";1");
                }
                if cell.attrs.contains(CellAttrs::DIM) {
                    sgr.push_str(";2");
                }
                if cell.attrs.contains(CellAttrs::ITALIC) {
                    sgr.push_str(";3");
                }
                if cell.attrs.contains(CellAttrs::UNDERLINE) {
                    sgr.push_str(";4");
                }
                if !cell.attrs.contains(CellAttrs::DEFAULT_FG) {
                    let _ = write!(sgr, ";38;2;{};{};{}", cell.fg.r, cell.fg.g, cell.fg.b);
                }
                if !cell.attrs.contains(CellAttrs::DEFAULT_BG) {
                    let _ = write!(sgr, ";48;2;{};{};{}", cell.bg.r, cell.bg.g, cell.bg.b);
                }
                sgr.push('m');
                out.extend_from_slice(sgr.as_bytes());
            }

            let mut buf = [0u8; 4];
            let s = cell.c.encode_utf8(&mut buf);
            out.extend_from_slice(s.as_bytes());
        }
    }

    out.extend_from_slice(b"\x1b[0m\r\n");
}

/// Feed preamble bytes into `term_state`, compute the resulting diff, and push
/// it into `scrollback` so clients that attach immediately after restore receive
/// the old visual content.
///
/// Calling `compute_diff()` here also synchronises `prev_cells` so the first
/// real PTY read does not re-emit the preamble content as a spurious diff.
fn seed_pane_with_preamble(
    term_state: &Arc<Mutex<TermState>>,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    seqno_counter: &Arc<AtomicU64>,
    preamble: &[u8],
) {
    if preamble.is_empty() {
        return;
    }

    let diff_opt = {
        let mut ts = term_state.lock().unwrap();
        ts.feed(preamble);
        match ts.compute_diff() {
            DiffResult::CellDiff(d) => Some(d),
            _ => None,
        }
    };

    if let Some(diff) = diff_opt {
        let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));
        scrollback.lock().unwrap().push(seqno, Arc::new(diff));
    }
}
