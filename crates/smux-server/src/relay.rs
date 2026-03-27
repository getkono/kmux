use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::session::PtyReader;
use smux_protocol::messages::{ClientId, SequenceNo, ServerMessage};
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
) {
    let seqno_counter = Arc::new(AtomicU64::new(1));
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
        diff: Arc::unwrap_or_clone(diff),
        seqno,
    };

    broadcast_to_clients(session, &msg, clients);
}

/// Send a message to all registered clients, handling backpressure and dead clients.
fn broadcast_to_clients(session: &str, msg: &ServerMessage, clients: &ClientMap) {
    let mut dead: Vec<ClientId> = Vec::new();

    {
        let map = clients.lock().unwrap();
        for (&client_id, tx) in map.iter() {
            match tx.try_send(msg.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    let lag_msg = ServerMessage::Lagged {
                        session: session.to_string(),
                        missed_count: 1,
                    };
                    let _ = tx.try_send(lag_msg);
                    warn!("Client {:?} lagged on session '{session}'", client_id);
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
