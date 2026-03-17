use smux::session::PtyReader;
use tokio::sync::broadcast;
use tracing::warn;

/// Read PTY output in a loop and broadcast every chunk to all attached clients.
///
/// This task owns the `PtyReader` half of a split session. It runs until EOF
/// or a read error, then exits silently. When all broadcast receivers are
/// dropped (no attached clients) the channel's `send` will return an error but
/// we continue reading -- the data is simply discarded until a new client
/// attaches and creates a new receiver.
pub async fn session_read_loop(reader: PtyReader, tx: broadcast::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // EOF -- PTY closed
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                // Ignore send errors -- no receivers attached is normal
                let _ = tx.send(chunk);
            }
            Err(e) => {
                warn!("PTY relay read error: {e}");
                break;
            }
        }
    }
}
