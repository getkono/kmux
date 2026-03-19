use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use smux::session::PtyReader;
use smux_protocol::messages::{SequenceNo, ServerMessage};
use tracing::warn;

use crate::app::ClientMap;
use crate::scrollback::SeqnoBuffer;

/// Read PTY output in a loop, tag each chunk with an incrementing sequence
/// number, append to scrollback, and forward to every registered client.
///
/// Dead clients are detected when their `mpsc::Sender` returns `TryFull` or
/// is dropped -- they are silently skipped; the connection task's cleanup will
/// remove them from the map on disconnect.
///
/// If a client's channel is full (`try_send` fails), we send them a `Lagged`
/// notification the next time their channel has room (best-effort).
pub async fn session_read_loop(
    mut reader: PtyReader,
    session: String,
    clients: ClientMap,
    scrollback: Arc<Mutex<SeqnoBuffer>>,
) {
    let seqno_counter = Arc::new(AtomicU64::new(1));
    let mut buf = vec![0u8; 4096];

    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF -- PTY closed
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                let seqno = SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));

                // Append to scrollback before delivering so a snapshot taken
                // immediately after attach cannot miss this chunk.
                scrollback.lock().unwrap().push(seqno, chunk.clone());

                // Deliver to each registered client independently.
                let msg = ServerMessage::PtyOutput {
                    session: session.clone(),
                    data: chunk,
                    seqno,
                };

                // Collect dead client IDs to remove after iteration.
                let mut dead: Vec<smux_protocol::messages::ClientId> = Vec::new();

                {
                    let map = clients.lock().unwrap();
                    for (&client_id, tx) in map.iter() {
                        match tx.try_send(msg.clone()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                // Channel is full; send Lagged notification if possible.
                                let lag_msg = ServerMessage::Lagged {
                                    session: session.clone(),
                                    missed_count: 1,
                                };
                                // Best-effort: ignore if still full.
                                let _ = tx.try_send(lag_msg);
                                warn!("Client {:?} lagged on session '{session}'", client_id);
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                dead.push(client_id);
                            }
                        }
                    }
                }

                // Clean up dead clients outside the lock to avoid deadlock.
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
