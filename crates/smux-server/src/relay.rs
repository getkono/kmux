use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::session::PtyReader;
use smux_protocol::messages::{ClientId, SequenceNo, ServerMessage, epoch_millis};
use tokio::time::{Duration, Instant};
use tracing::warn;

use crate::app::ClientMap;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Coalescing window: accumulate PTY bytes for up to this long before
/// flushing a diff. Keeps interactive latency imperceptible (~4ms) while
/// batching burst output into fewer, larger diffs.
const COALESCE_WINDOW: Duration = Duration::from_millis(4);

/// Flush immediately if the accumulator reaches this size, regardless of
/// the coalescing timer.
const COALESCE_MAX_BYTES: usize = 32_768;

/// Read PTY output in a loop, feed bytes through server-side VT emulation,
/// compute cell-level diffs, and forward `TerminalUpdate` messages to every
/// registered client.
///
/// Uses a coalescing window to batch rapid PTY reads into a single diff,
/// reducing message frequency during burst output (cat, make, vim quit).
/// Diffs with no changes (no cell, cursor, or mode changes) are skipped.
pub async fn session_diff_loop(
    mut reader: PtyReader,
    session: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
    seqno_counter: Arc<AtomicU64>,
) {
    let mut buf = vec![0u8; 4096];
    let mut accum: Vec<u8> = Vec::new();
    let mut deadline: Option<Instant> = None;

    loop {
        // If we have accumulated bytes, wait for either more data or the
        // coalescing timer to expire. Otherwise, just wait for data.
        let flush_timer = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;  // prefer reading data over flushing

            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        // EOF -- flush remaining bytes and exit
                        if !accum.is_empty() {
                            flush_diff(
                                &accum, &session, &term_state, &scrollback,
                                &clients, &seqno_counter,
                            );
                        }
                        break;
                    }
                    Ok(n) => {
                        accum.extend_from_slice(&buf[..n]);
                        if deadline.is_none() {
                            deadline = Some(Instant::now() + COALESCE_WINDOW);
                        }
                        // Flush immediately if accumulator is large enough
                        if accum.len() >= COALESCE_MAX_BYTES {
                            flush_diff(
                                &accum, &session, &term_state, &scrollback,
                                &clients, &seqno_counter,
                            );
                            accum.clear();
                            deadline = None;
                        }
                    }
                    Err(e) => {
                        warn!("PTY relay read error: {e}");
                        if !accum.is_empty() {
                            flush_diff(
                                &accum, &session, &term_state, &scrollback,
                                &clients, &seqno_counter,
                            );
                        }
                        break;
                    }
                }
            }

            _ = flush_timer => {
                // Coalescing timer expired -- flush accumulated bytes
                flush_diff(
                    &accum, &session, &term_state, &scrollback,
                    &clients, &seqno_counter,
                );
                accum.clear();
                deadline = None;
            }
        }
    }
}

/// Feed accumulated bytes through VTE, compute diff, and broadcast to clients.
fn flush_diff(
    data: &[u8],
    session: &str,
    term_state: &Arc<Mutex<TermState>>,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    clients: &ClientMap,
    seqno_counter: &Arc<AtomicU64>,
) {
    let diff = {
        let mut ts = term_state.lock().unwrap();
        ts.feed(data);
        ts.compute_diff()
    };

    let Some(diff) = diff else {
        return;
    };

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
