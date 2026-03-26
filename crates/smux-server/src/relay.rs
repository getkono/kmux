use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::session::PtyReader;
use smux_protocol::messages::{SequenceNo, ServerMessage};
use tracing::warn;

use crate::app::ClientMap;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Read PTY output in a loop, feed bytes through server-side VT emulation,
/// compute cell-level diffs, and forward `TerminalUpdate` messages to every
/// registered client.
///
/// Empty diffs (no cell changes) are skipped to avoid unnecessary network traffic.
pub async fn session_diff_loop(
    mut reader: PtyReader,
    session: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
) {
    let seqno_counter = Arc::new(AtomicU64::new(1));
    let mut buf = vec![0u8; 4096];

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF -- PTY closed
            Ok(n) => {
                let chunk = &buf[..n];

                // Feed bytes into the server-side VTE emulator and compute diff
                let diff = {
                    let mut ts = term_state.lock().unwrap();
                    ts.feed(chunk);
                    ts.compute_diff()
                };

                // Skip empty diffs (no visible changes).
                // Seqno is assigned *after* this check so empty diffs don't
                // create gaps in the sequence stream.
                if diff.ops.is_empty() {
                    continue;
                }

                let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));

                // Wrap in Arc to avoid deep-cloning for scrollback storage
                let diff = Arc::new(diff);
                scrollback.lock().unwrap().push(seqno, Arc::clone(&diff));

                let msg = ServerMessage::TerminalUpdate {
                    session: session.clone(),
                    diff: Arc::unwrap_or_clone(diff),
                    seqno,
                };

                // Deliver to each registered client
                let mut dead: Vec<smux_protocol::messages::ClientId> = Vec::new();

                {
                    let map = clients.lock().unwrap();
                    for (&client_id, tx) in map.iter() {
                        match tx.try_send(msg.clone()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                let lag_msg = ServerMessage::Lagged {
                                    session: session.clone(),
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
            Err(e) => {
                warn!("PTY relay read error: {e}");
                break;
            }
        }
    }
}
