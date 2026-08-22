//! Read-only queries and out-of-band notifications: process overview, log
//! streaming, directory listing, desktop notifications, and the ping round-trip.
//!
//! Grouped because none of them touches session state — they answer questions
//! about the daemon or the host, or push something at the user.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use kmux_protocol::messages::{
    AttentionKind, DirEntry, ErrorCode, PaneId, RequestId, ServerMessage, epoch_millis,
};

use crate::connection::classify_error;

use super::super::SharedClientState;

/// Handle [`ClientMessage::ProcessOverview`].
pub(super) async fn on_process_overview(state: &mut SharedClientState, request_id: RequestId) {
    // Merge the locally-hosted panes' process trees with every open
    // peer's (issue #122). Federation off ⇒ the federated half is empty.
    let mut panes = state.app.local_process_overview().await;
    panes.extend(state.app.collect_federated_process_overview().await);
    state.send(ServerMessage::ProcessOverviewResult { request_id, panes });
}

/// Handle [`ClientMessage::ListDirectory`].
pub(super) fn on_list_directory(state: &mut SharedClientState, request_id: RequestId, path: &str) {
    state.send(list_directory(request_id, path));
}

/// Handle [`ClientMessage::Notify`].
pub(super) async fn on_notify(
    state: &mut SharedClientState,
    request_id: RequestId,
    pane_id: PaneId,
    kind: AttentionKind,
    title: String,
    body: String,
) {
    match state
        .app
        .notify_pane_attention(&pane_id, kind, title, body)
        .await
    {
        Ok(()) => state.send(ServerMessage::NotifyAccepted { request_id }),
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::Ping`].
pub(super) fn on_ping(state: &mut SharedClientState, seq: u64) {
    state.send(ServerMessage::Pong { seq });
}

/// Handle [`ClientMessage::Pong`].
pub(super) fn on_pong(state: &mut SharedClientState, seq: u64) {
    let sent = *state.metrics.last_ping_sent.lock().unwrap();
    if let Some((sent_seq, sent_at)) = sent
        && sent_seq == seq
    {
        let rtt_ms = sent_at.elapsed().as_millis() as u64;
        state.metrics.last_rtt_ms.store(rtt_ms, Ordering::Relaxed);
        state
            .metrics
            .last_pong_ms
            .store(epoch_millis(), Ordering::Relaxed);
    }
}

/// Chunk size for streaming a log file to a client (issue #187): large enough
/// that framing overhead is negligible, small enough to bound per-message memory.
const LOG_CHUNK_BYTES: usize = 64 * 1024;

/// Answer a [`ClientMessage::FetchLogs`] (issue #187): stream this daemon's own
/// log file to the client over the control channel.
///
/// Sends the existing content (trimmed to the last `lines` lines when set) as
/// `LogChunk`s, then either a terminating `LogEnd` or — under `follow` — spawns a
/// detached task that tails the file and keeps pushing `LogChunk`s until the
/// connection's writer is gone (its `ctrl_tx` is closed). The follow task checks
/// `ctrl_tx.is_closed()` each tick so a disconnect during an idle log never
/// leaks the task.
pub(super) async fn on_fetch_logs(
    state: &SharedClientState,
    request_id: u64,
    lines: Option<u32>,
    follow: bool,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let path = match kmux_sys::dirs::daemon_log_path() {
        Ok(p) => p,
        Err(e) => {
            state.error(
                Some(request_id),
                ErrorCode::InternalError,
                format!("daemon log path unavailable: {e}"),
            );
            return;
        }
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            state.error(
                Some(request_id),
                ErrorCode::InternalError,
                format!("daemon log not readable at {}: {e}", path.display()),
            );
            return;
        }
    };

    let mut buf = Vec::new();
    if let Err(e) = file.read_to_end(&mut buf).await {
        state.error(
            Some(request_id),
            ErrorCode::InternalError,
            format!("reading daemon log failed: {e}"),
        );
        return;
    }

    let start = match lines {
        Some(n) => kmux_sys::log_tail::last_n_lines_offset(&buf, n as usize),
        None => 0,
    };
    for chunk in buf[start..].chunks(LOG_CHUNK_BYTES) {
        state.send(ServerMessage::LogChunk {
            request_id,
            data: chunk.to_vec(),
        });
    }

    if !follow {
        state.send(ServerMessage::LogEnd { request_id });
        return;
    }

    // Follow: tail appended bytes from the current end of file. `read_to_end`
    // already left the cursor at EOF, but seek explicitly to be sure.
    let ctrl_tx = state.ctrl_tx.clone();
    tokio::spawn(async move {
        if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
            return;
        }
        let mut read_buf = vec![0u8; LOG_CHUNK_BYTES];
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            if ctrl_tx.is_closed() {
                return;
            }
            match file.read(&mut read_buf).await {
                Ok(0) => continue,
                Ok(n) => {
                    if ctrl_tx
                        .send(ServerMessage::LogChunk {
                            request_id,
                            data: read_buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
}

/// Maximum number of directory entries returned in a single `DirectoryListing`,
/// to bound the reply size for very large directories.
const MAX_DIR_ENTRIES: usize = 2000;

/// Build the `DirectoryListing` reply for a `ListDirectory` request.
///
/// Resolves `requested` (empty ⇒ `$HOME`, else the daemon's `.`), canonicalizes
/// it, and returns its **subdirectories only** (the browser is choosing a
/// directory), sorted case-insensitively and capped at [`MAX_DIR_ENTRIES`]. On
/// any IO error it returns `error: Some(..)` with empty `entries` and echoes the
/// requested path so the client keeps showing where it tried to go. This reads
/// the daemon's own filesystem (the user owns it), so no sandboxing is applied
/// beyond normal filesystem permissions.
pub(super) fn list_directory(request_id: u64, requested: &str) -> ServerMessage {
    let target = if requested.is_empty() {
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
    } else {
        PathBuf::from(requested)
    };

    let canonical = match std::fs::canonicalize(&target) {
        Ok(p) => p,
        Err(e) => return directory_error(request_id, requested, &e),
    };

    let read = match std::fs::read_dir(&canonical) {
        Ok(rd) => rd,
        Err(e) => return directory_error(request_id, requested, &e),
    };

    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in read.flatten() {
        // Skip entries whose metadata can't be read (e.g. dangling symlink) and
        // anything that is not a directory — `file_type()` does not traverse
        // symlinks, so a symlink loop can't recurse here.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            Ok(ft) if ft.is_symlink() => {
                // Resolve one level so symlinked directories still appear, but
                // bail out gracefully if the link is broken or loops.
                match std::fs::metadata(entry.path()) {
                    Ok(md) if md.is_dir() => {}
                    _ => continue,
                }
            }
            _ => continue,
        }
        if let Some(name) = entry.file_name().to_str() {
            entries.push(DirEntry {
                name: name.to_string(),
                is_dir: true,
            });
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries.truncate(MAX_DIR_ENTRIES);

    let parent = canonical
        .parent()
        .and_then(Path::to_str)
        .map(str::to_string);

    ServerMessage::DirectoryListing {
        request_id,
        path: canonical.to_string_lossy().into_owned(),
        parent,
        entries,
        error: None,
    }
}

/// Build a failed `DirectoryListing` echoing the requested path.
fn directory_error(request_id: u64, requested: &str, err: &std::io::Error) -> ServerMessage {
    ServerMessage::DirectoryListing {
        request_id,
        path: requested.to_string(),
        parent: None,
        entries: vec![],
        error: Some(err.to_string()),
    }
}
