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
    pane_id: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
    seqno_counter: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 65536];
    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let cycle_start = Instant::now();
                let mut total_bytes = n;
                let mut ts = term_state.lock().unwrap();
                ts.feed(&buf[..n]);
                // Coalesce: drain all immediately-available PTY output before
                // computing the diff, so burst output (e.g. vim exit, large
                // cat) produces a single diff instead of many intermediate ones.
                loop {
                    match reader.try_read(&mut buf) {
                        Ok(0) => break,
                        Ok(m) => {
                            ts.feed(&buf[..m]);
                            total_bytes += m;
                        }
                        Err(_) => break, // WouldBlock or error
                    }
                }
                drop(ts);
                flush_cell_diff(
                    &pane_id,
                    &term_state,
                    &scrollback,
                    &clients,
                    &seqno_counter,
                    &mut prev_cursor,
                    &mut prev_modes,
                );
                let cycle_us = cycle_start.elapsed().as_micros();
                debug!(
                    pane_id,
                    bytes = total_bytes,
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
    pane_id: &str,
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
                pane_id,
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
                pane_id: pane_id.to_string(),
                diff,
                seqno,
                sent_at_ms: epoch_millis(),
            };
            broadcast_to_clients(pane_id, &msg, clients, term_state, seqno);
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
                    pane_id: pane_id.to_string(),
                    cursor,
                    modes,
                    seqno,
                    sent_at_ms: epoch_millis(),
                };
                broadcast_to_clients(pane_id, &msg, clients, term_state, seqno);
            }
        }
        DiffResult::None => {
            debug!(pane_id, "flush_cell_diff: no changes");
        }
    }
}

/// Send a message to all registered clients, handling backpressure and dead clients.
fn broadcast_to_clients(
    pane_id: &str,
    msg: &ServerMessage,
    clients: &ClientMap,
    term_state: &Arc<Mutex<TermState>>,
    seqno: SequenceNo,
) {
    let mut dead: Vec<ClientId> = Vec::new();
    let mut snapshot_msg: Option<ServerMessage> = None;

    {
        let map = clients.lock().unwrap();
        for (&client_id, sender) in map.iter() {
            let outgoing = if sender.force_full_snapshot {
                snapshot_msg.get_or_insert_with(|| {
                    let snapshot = term_state.lock().unwrap().snapshot();
                    ServerMessage::TerminalSnapshot {
                        pane_id: pane_id.to_string(),
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
                        pane_id: pane_id.to_string(),
                        missed_count: 1,
                    });
                    dead.push(client_id);
                    warn!(
                        "Client {:?} lagged on pane '{pane_id}', sending Lagged via ctrl",
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

    fn dummy_update(pane_id: &str) -> ServerMessage {
        ServerMessage::TerminalUpdate {
            pane_id: pane_id.to_string(),
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
        use crate::backend::{
            BackendConfig, BackendSize, CapabilityHandles, DEFAULT_SCROLLBACK, NullEventSink,
        };
        use std::sync::Arc;
        Arc::new(Mutex::new(new_term_state(BackendConfig {
            size: BackendSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            capabilities: CapabilityHandles {
                kitty_graphics: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                kitty_keyboard: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            events: Arc::new(NullEventSink),
            scrollback: DEFAULT_SCROLLBACK,
        })))
    }

    #[test]
    fn broadcast_sends_lagged_via_ctrl_when_data_full() {
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        data_tx.try_send(dummy_update("eagle/0")).unwrap();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &ts,
            SequenceNo(1),
        );

        let msg = ctrl_rx.try_recv().expect("should receive Lagged on ctrl");
        assert!(
            matches!(&msg, ServerMessage::Lagged { pane_id, .. } if pane_id == "eagle/0"),
            "expected Lagged message, got {:?}",
            msg
        );
    }

    #[test]
    fn broadcast_removes_client_after_full() {
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        data_tx.try_send(dummy_update("eagle/0")).unwrap();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(42),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &ts,
            SequenceNo(1),
        );

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
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &ts,
            SequenceNo(1),
        );

        let msg = data_rx
            .try_recv()
            .expect("should receive message on data channel");
        assert!(matches!(msg, ServerMessage::TerminalUpdate { .. }));

        assert_eq!(clients.lock().unwrap().len(), 1);
    }
}
