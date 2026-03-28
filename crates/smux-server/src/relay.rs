use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::session::PtyReader;
use smux_protocol::messages::{
    ClientId, CursorState, SequenceNo, ServerMessage, TermModes, TerminalDiff, epoch_millis,
};
use tokio::time::{Duration, Instant};
use tracing::{debug, warn};

use crate::app::ClientMap;
use crate::diff_engine::DiffResult;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Coalescing window: accumulate cell diffs for up to this long before
/// flushing. Keeps interactive latency imperceptible (~4ms) while
/// batching burst output into fewer, larger diffs.
const COALESCE_WINDOW: Duration = Duration::from_millis(4);

/// Flush immediately if accumulated bytes since last cell diff exceed this.
const COALESCE_MAX_BYTES: usize = 32_768;

/// Read PTY output in a loop, feed bytes immediately through server-side
/// VT emulation, send cursor-only updates on the fast-path, and coalesce
/// cell diffs on a timer.
///
/// Key design points:
/// - `feed()` is called on every PTY read (no byte accumulation)
/// - Cursor/mode changes are detected after each `feed()` and broadcast
///   immediately as `CursorUpdate` messages (fast-path)
/// - Cell diffs are coalesced on a timer to batch burst output
/// - No `biased` select — fair polling prevents timer starvation
pub async fn session_diff_loop(
    mut reader: PtyReader,
    session: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
    seqno_counter: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 4096];
    let mut cells_dirty = false;
    let mut deadline = Instant::now();
    let mut bytes_since_diff: usize = 0;
    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;

    loop {
        if !cells_dirty {
            // Nothing pending: block on PTY read
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut ts = term_state.lock().unwrap();
                    ts.feed(&buf[..n]);
                    check_and_send_cursor(
                        &ts,
                        &mut prev_cursor,
                        &mut prev_modes,
                        &session,
                        &scrollback,
                        &clients,
                        &seqno_counter,
                    );
                    drop(ts);
                    cells_dirty = true;
                    bytes_since_diff = n;
                    deadline = Instant::now() + COALESCE_WINDOW;
                }
                Err(e) => {
                    warn!("PTY relay read error: {e}");
                    break;
                }
            }
        } else {
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::select! {
                result = reader.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            flush_cell_diff(
                                &session, &term_state, &scrollback,
                                &clients, &seqno_counter,
                                &mut prev_cursor, &mut prev_modes,
                            );
                            break;
                        }
                        Ok(n) => {
                            let mut ts = term_state.lock().unwrap();
                            ts.feed(&buf[..n]);
                            check_and_send_cursor(
                                &ts,
                                &mut prev_cursor,
                                &mut prev_modes,
                                &session,
                                &scrollback,
                                &clients,
                                &seqno_counter,
                            );
                            drop(ts);
                            bytes_since_diff += n;
                            if bytes_since_diff >= COALESCE_MAX_BYTES {
                                flush_cell_diff(
                                    &session, &term_state, &scrollback,
                                    &clients, &seqno_counter,
                                    &mut prev_cursor, &mut prev_modes,
                                );
                                cells_dirty = false;
                                bytes_since_diff = 0;
                            }
                        }
                        Err(e) => {
                            warn!("PTY relay read error: {e}");
                            flush_cell_diff(
                                &session, &term_state, &scrollback,
                                &clients, &seqno_counter,
                                &mut prev_cursor, &mut prev_modes,
                            );
                            break;
                        }
                    }
                }

                _ = tokio::time::sleep(remaining) => {
                    flush_cell_diff(
                        &session, &term_state, &scrollback,
                        &clients, &seqno_counter,
                        &mut prev_cursor, &mut prev_modes,
                    );
                    cells_dirty = false;
                    bytes_since_diff = 0;
                }
            }
        }
    }
}

/// Cursor fast-path: after each `feed()`, compare cursor/modes against
/// tracked state and broadcast `CursorUpdate` if changed.
///
/// This does NOT call `fill_cells()` — it only reads `backend.cursor()`
/// and `backend.modes()`, which is much cheaper.
fn check_and_send_cursor(
    ts: &TermState,
    prev_cursor: &mut CursorState,
    prev_modes: &mut TermModes,
    session: &str,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    clients: &ClientMap,
    seqno_counter: &Arc<AtomicU64>,
) {
    let cursor = ts.cursor();
    let modes = ts.modes();
    if cursor != *prev_cursor || modes != *prev_modes {
        *prev_cursor = cursor;
        *prev_modes = modes;
        let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));

        // Store as an empty-ops TerminalDiff for scrollback replay
        scrollback.lock().unwrap().push(
            seqno,
            Arc::new(TerminalDiff {
                ops: vec![],
                cursor,
                modes,
            }),
        );

        let msg = ServerMessage::CursorUpdate {
            session: session.to_string(),
            cursor,
            modes,
            seqno,
            sent_at_ms: epoch_millis(),
        };
        broadcast_to_clients(session, &msg, clients);
    }
}

/// Coalescing timer expired or byte threshold hit — compute full cell diff.
fn flush_cell_diff(
    session: &str,
    term_state: &Arc<Mutex<TermState>>,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    clients: &ClientMap,
    seqno_counter: &Arc<AtomicU64>,
    prev_cursor: &mut CursorState,
    prev_modes: &mut TermModes,
) {
    let result = {
        let mut ts = term_state.lock().unwrap();
        ts.compute_diff()
    };

    match result {
        DiffResult::CellDiff(diff) => {
            *prev_cursor = diff.cursor;
            *prev_modes = diff.modes;

            debug!(
                session,
                ops = diff.ops.len(),
                cursor_row = diff.cursor.row,
                cursor_col = diff.cursor.col,
                "flush_cell_diff: broadcasting cell diff"
            );

            let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));
            let diff = Arc::new(diff);
            scrollback.lock().unwrap().push(seqno, Arc::clone(&diff));

            let msg = ServerMessage::TerminalUpdate {
                session: session.to_string(),
                diff,
                seqno,
                sent_at_ms: epoch_millis(),
            };
            broadcast_to_clients(session, &msg, clients);
        }
        DiffResult::CursorOnly { cursor, modes } => {
            // Cursor may have already been sent by fast-path; compare again
            if cursor != *prev_cursor || modes != *prev_modes {
                *prev_cursor = cursor;
                *prev_modes = modes;
                let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));
                scrollback.lock().unwrap().push(
                    seqno,
                    Arc::new(TerminalDiff {
                        ops: vec![],
                        cursor,
                        modes,
                    }),
                );
                let msg = ServerMessage::CursorUpdate {
                    session: session.to_string(),
                    cursor,
                    modes,
                    seqno,
                    sent_at_ms: epoch_millis(),
                };
                broadcast_to_clients(session, &msg, clients);
            }
        }
        DiffResult::None => {
            debug!(session, "flush_cell_diff: no changes");
        }
    }
}

/// Send a message to all registered clients, handling backpressure and dead clients.
///
/// When a client's data channel is full, send `Lagged` via the unbounded control
/// channel (which never fails) and remove the data sender so the uni-stream writer
/// exits cleanly. The client receives the `Lagged` notification reliably and can
/// re-attach for a fresh snapshot.
fn broadcast_to_clients(session: &str, msg: &ServerMessage, clients: &ClientMap) {
    let mut dead: Vec<ClientId> = Vec::new();

    {
        let map = clients.lock().unwrap();
        for (&client_id, sender) in map.iter() {
            match sender.data_tx.try_send(msg.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Send Lagged via the control channel (unbounded, never fails).
                    let _ = sender.ctrl_tx.send(ServerMessage::Lagged {
                        session: session.to_string(),
                        missed_count: 1,
                    });
                    // Remove data sender so uni-stream writer task exits cleanly.
                    dead.push(client_id);
                    warn!(
                        "Client {:?} lagged on session '{session}', sending Lagged via ctrl",
                        client_id
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    dead.push(client_id);
                }
            }
        }
    }

    if !dead.is_empty() {
        let mut map = clients.lock().unwrap();
        for id in dead {
            map.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::app::ClientSender;
    use smux_protocol::messages::{CursorState, TermModes, TerminalDiff};
    use tokio::sync::mpsc;

    fn dummy_update(session: &str) -> ServerMessage {
        ServerMessage::TerminalUpdate {
            session: session.to_string(),
            diff: Arc::new(TerminalDiff {
                ops: vec![],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
            }),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        }
    }

    #[test]
    fn broadcast_sends_lagged_via_ctrl_when_data_full() {
        // Create a data channel with capacity 1, and fill it.
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        // Fill the data channel
        data_tx.try_send(dummy_update("test")).unwrap();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients
            .lock()
            .unwrap()
            .insert(ClientId(1), ClientSender { data_tx, ctrl_tx });

        // Now broadcast — data channel is full
        broadcast_to_clients("test", &dummy_update("test"), &clients);

        // Lagged should arrive on the ctrl channel
        let msg = ctrl_rx.try_recv().expect("should receive Lagged on ctrl");
        assert!(
            matches!(&msg, ServerMessage::Lagged { session, .. } if session == "test"),
            "expected Lagged message, got {:?}",
            msg
        );
    }

    #[test]
    fn broadcast_removes_client_after_full() {
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        // Fill the data channel
        data_tx.try_send(dummy_update("test")).unwrap();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients
            .lock()
            .unwrap()
            .insert(ClientId(42), ClientSender { data_tx, ctrl_tx });

        broadcast_to_clients("test", &dummy_update("test"), &clients);

        // Client should be removed from the map
        assert!(
            clients.lock().unwrap().is_empty(),
            "lagged client should be removed from map"
        );
    }

    #[test]
    fn broadcast_delivers_to_healthy_client() {
        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(16);
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients
            .lock()
            .unwrap()
            .insert(ClientId(1), ClientSender { data_tx, ctrl_tx });

        broadcast_to_clients("test", &dummy_update("test"), &clients);

        // Message should arrive on data channel
        let msg = data_rx
            .try_recv()
            .expect("should receive message on data channel");
        assert!(matches!(msg, ServerMessage::TerminalUpdate { .. }));

        // Client should still be in the map
        assert_eq!(clients.lock().unwrap().len(), 1);
    }
}
