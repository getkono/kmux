use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use kmux_protocol::messages::{
    ClientId, CursorState, SequenceNo, ServerMessage, TermModes, TerminalDiff, epoch_millis,
};
use kmux_pty::session::PtyReader;
use tracing::{debug, warn};

use crate::app::ClientMap;
use crate::diff_engine::DiffResult;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Read PTY output in a loop, feed bytes through server-side VT emulation,
/// and immediately compute + broadcast cell diffs after each read.
pub async fn session_diff_loop(
    mut reader: PtyReader,
    session: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
    seqno_counter: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 4096];
    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let cycle_start = Instant::now();
                let mut ts = term_state.lock().unwrap();
                ts.feed(&buf[..n]);
                drop(ts);
                flush_cell_diff(
                    &session,
                    &term_state,
                    &scrollback,
                    &clients,
                    &seqno_counter,
                    &mut prev_cursor,
                    &mut prev_modes,
                );
                let cycle_us = cycle_start.elapsed().as_micros();
                debug!(
                    session,
                    bytes = n,
                    cycle_us,
                    "PTY read-diff-broadcast cycle"
                );
            }
            Err(e) => {
                warn!("PTY relay read error: {e}");
                break;
            }
        }
    }
}

/// Compute cell diff and broadcast to clients.
fn flush_cell_diff(
    session: &str,
    term_state: &Arc<Mutex<TermState>>,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    clients: &ClientMap,
    seqno_counter: &Arc<AtomicU64>,
    prev_cursor: &mut CursorState,
    prev_modes: &mut TermModes,
) {
    let diff_start = Instant::now();
    let result = {
        let mut ts = term_state.lock().unwrap();
        ts.compute_diff()
    };
    let diff_us = diff_start.elapsed().as_micros();

    match result {
        DiffResult::CellDiff(diff) => {
            *prev_cursor = diff.cursor;
            *prev_modes = diff.modes;

            let ops = diff.ops.len();
            debug!(
                session,
                ops,
                diff_us,
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
            broadcast_to_clients(session, &msg, clients, term_state, seqno);
        }
        DiffResult::CursorOnly { cursor, modes } => {
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
                        scrollback_lines: vec![],
                    }),
                );
                let msg = ServerMessage::CursorUpdate {
                    session: session.to_string(),
                    cursor,
                    modes,
                    seqno,
                    sent_at_ms: epoch_millis(),
                };
                broadcast_to_clients(session, &msg, clients, term_state, seqno);
            }
        }
        DiffResult::None => {
            debug!(session, "flush_cell_diff: no changes");
        }
    }
}

/// Send a message to all registered clients, handling backpressure and dead clients.
///
/// Clients with `force_full_snapshot` enabled receive a `TerminalSnapshot` instead
/// of the incremental diff message. The snapshot is generated lazily (only when at
/// least one forced client exists).
///
/// When a client's data channel is full, send `Lagged` via the unbounded control
/// channel (which never fails) and remove the data sender so the uni-stream writer
/// exits cleanly. The client receives the `Lagged` notification reliably and can
/// re-attach for a fresh snapshot.
fn broadcast_to_clients(
    session: &str,
    msg: &ServerMessage,
    clients: &ClientMap,
    term_state: &Arc<Mutex<TermState>>,
    seqno: SequenceNo,
) {
    let mut dead: Vec<ClientId> = Vec::new();
    // Lazily computed snapshot for clients in forced-snapshot mode.
    let mut snapshot_msg: Option<ServerMessage> = None;

    {
        let map = clients.lock().unwrap();
        for (&client_id, sender) in map.iter() {
            let outgoing = if sender.force_full_snapshot {
                snapshot_msg.get_or_insert_with(|| {
                    let snapshot = term_state.lock().unwrap().snapshot();
                    ServerMessage::TerminalSnapshot {
                        session: session.to_string(),
                        snapshot,
                        seqno,
                        sent_at_ms: epoch_millis(),
                    }
                })
            } else {
                msg
            };

            match sender.data_tx.try_send(outgoing.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    let _ = sender.ctrl_tx.send(ServerMessage::Lagged {
                        session: session.to_string(),
                        missed_count: 1,
                    });
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
    use crate::term_state::new_term_state;
    use kmux_protocol::messages::{CursorState, TermModes, TerminalDiff};
    use tokio::sync::mpsc;

    fn dummy_update(session: &str) -> ServerMessage {
        ServerMessage::TerminalUpdate {
            session: session.to_string(),
            diff: Arc::new(TerminalDiff {
                ops: vec![],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                scrollback_lines: vec![],
            }),
            seqno: SequenceNo(1),
            sent_at_ms: 0,
        }
    }

    fn test_term_state() -> Arc<Mutex<TermState>> {
        Arc::new(Mutex::new(new_term_state(24, 80)))
    }

    #[test]
    fn broadcast_sends_lagged_via_ctrl_when_data_full() {
        // Create a data channel with capacity 1, and fill it.
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        // Fill the data channel
        data_tx.try_send(dummy_update("test")).unwrap();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
            },
        );

        // Now broadcast — data channel is full
        let ts = test_term_state();
        broadcast_to_clients("test", &dummy_update("test"), &clients, &ts, SequenceNo(1));

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
        clients.lock().unwrap().insert(
            ClientId(42),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
            },
        );

        let ts = test_term_state();
        broadcast_to_clients("test", &dummy_update("test"), &clients, &ts, SequenceNo(1));

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
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
            },
        );

        let ts = test_term_state();
        broadcast_to_clients("test", &dummy_update("test"), &clients, &ts, SequenceNo(1));

        // Message should arrive on data channel
        let msg = data_rx
            .try_recv()
            .expect("should receive message on data channel");
        assert!(matches!(msg, ServerMessage::TerminalUpdate { .. }));

        // Client should still be in the map
        assert_eq!(clients.lock().unwrap().len(), 1);
    }
}
