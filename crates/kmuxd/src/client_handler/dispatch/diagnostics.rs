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

/// Handle [`ClientMessage::ProcessOverview`](kmux_protocol::messages::ClientMessage::ProcessOverview).
pub(super) async fn on_process_overview(state: &mut SharedClientState, request_id: RequestId) {
    // Merge the locally-hosted panes' process trees with every open
    // peer's (issue #122). Federation off ⇒ the federated half is empty.
    let mut panes = state.app.local_process_overview().await;
    panes.extend(state.app.collect_federated_process_overview().await);
    state.send(ServerMessage::ProcessOverviewResult { request_id, panes });
}

/// Handle [`ClientMessage::ListDirectory`](kmux_protocol::messages::ClientMessage::ListDirectory).
pub(super) fn on_list_directory(state: &mut SharedClientState, request_id: RequestId, path: &str) {
    state.send(list_directory(request_id, path));
}

/// Handle [`ClientMessage::Notify`](kmux_protocol::messages::ClientMessage::Notify).
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

/// Handle [`ClientMessage::Ping`](kmux_protocol::messages::ClientMessage::Ping).
pub(super) fn on_ping(state: &mut SharedClientState, seq: u64) {
    state.send(ServerMessage::Pong { seq });
}

/// Handle [`ClientMessage::Pong`](kmux_protocol::messages::ClientMessage::Pong).
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

/// Answer a [`ClientMessage::FetchLogs`](kmux_protocol::messages::ClientMessage::FetchLogs) (issue #187): stream this daemon's own
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

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::{Path, list_directory};

    #[tokio::test]
    async fn process_overview_on_an_empty_server_returns_no_panes() {
        let (keep, msgs) = dispatch_one(ClientMessage::ProcessOverview { request_id: 12 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::ProcessOverviewResult { request_id, panes } => {
                assert_eq!(request_id, 12);
                assert!(panes.is_empty(), "no panes exist: {panes:?}");
            }
            other => panic!("expected ProcessOverviewResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_logs_answers_a_request_correlated_terminated_stream() {
        let (keep, msgs) = dispatch_one(ClientMessage::FetchLogs {
            request_id: 21,
            lines: Some(5),
            follow: false,
        })
        .await;
        assert!(keep);
        // Whether the daemon log file exists depends on the machine's state dir,
        // so the pinned invariants are the ones the arm controls: every reply
        // carries this request id, and the stream is terminated exactly once —
        // by `LogEnd` when the log was readable, by an `Error` when it was not.
        assert!(!msgs.is_empty(), "the arm always answers");
        for msg in &msgs {
            let id = match msg {
                ServerMessage::LogChunk { request_id, .. }
                | ServerMessage::LogEnd { request_id } => Some(*request_id),
                ServerMessage::Error { request_id, .. } => *request_id,
                other => panic!("unexpected FetchLogs reply {other:?}"),
            };
            assert_eq!(id, Some(21), "reply not correlated: {msg:?}");
        }
        match msgs.last().expect("non-empty asserted above") {
            ServerMessage::LogEnd { .. } => {}
            ServerMessage::Error { code, .. } => assert_eq!(*code, ErrorCode::InternalError),
            other => panic!("stream must end with LogEnd or Error, got {other:?}"),
        }
    }

    #[test]
    fn list_directory_returns_sorted_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("zebra")).unwrap();
        std::fs::create_dir(tmp.path().join("Alpha")).unwrap();
        std::fs::write(tmp.path().join("a_file.txt"), b"hi").unwrap();

        let msg = list_directory(1, tmp.path().to_str().unwrap());
        match msg {
            ServerMessage::DirectoryListing {
                request_id,
                entries,
                error,
                parent,
                ..
            } => {
                assert_eq!(request_id, 1);
                assert!(error.is_none());
                assert!(parent.is_some(), "a tempdir has a parent");
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                // Files are excluded; dirs are sorted case-insensitively.
                assert_eq!(names, vec!["Alpha", "zebra"]);
                assert!(entries.iter().all(|e| e.is_dir));
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn list_directory_reports_error_for_missing_path() {
        let msg = list_directory(2, "/this/path/does/not/exist/kmux");
        match msg {
            ServerMessage::DirectoryListing {
                path,
                entries,
                error,
                ..
            } => {
                assert_eq!(path, "/this/path/does/not/exist/kmux");
                assert!(entries.is_empty());
                assert!(error.is_some());
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn list_directory_empty_path_resolves_a_default() {
        // An empty path resolves to $HOME (or "."); either way it must not error
        // in a normal environment and must echo a canonical, absolute path.
        let msg = list_directory(3, "");
        match msg {
            ServerMessage::DirectoryListing { path, error, .. } => {
                assert!(error.is_none(), "default dir should list: {error:?}");
                assert!(
                    Path::new(&path).is_absolute(),
                    "canonicalized path should be absolute: {path}"
                );
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_directory_of_a_missing_path_answers_a_listing_carrying_the_error() {
        let (keep, msgs) = dispatch_one(ClientMessage::ListDirectory {
            request_id: 15,
            path: "/this/path/does/not/exist/kmux".to_string(),
        })
        .await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::DirectoryListing {
                request_id,
                path,
                parent,
                entries,
                error,
            } => {
                assert_eq!(request_id, 15);
                // The requested path is echoed back verbatim, not canonicalized.
                assert_eq!(path, "/this/path/does/not/exist/kmux");
                assert_eq!(parent, None);
                assert!(entries.is_empty());
                assert!(error.is_some(), "the IO failure is reported inline");
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_for_an_unknown_pane_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Notify {
            request_id: 20,
            pane_id: MISSING_PANE.to_string(),
            kind: AttentionKind::TurnDone,
            title: "title".to_string(),
            body: "body".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(20));
        // The protocol doc for `Notify` promises an error when "the pane is
        // unknown", and this is the code that says so.
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn ping_is_answered_with_a_pong_carrying_the_same_seq() {
        let (keep, msgs) = dispatch_one(ClientMessage::Ping { seq: 7 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::Pong { seq } => assert_eq!(seq, 7),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unsolicited_pong_answers_nothing_and_records_no_rtt() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(&mut state, ClientMessage::Pong { seq: 7 }, &NoopAttacher).await;
        assert!(keep);
        assert!(drain(&mut ctrl_rx).is_empty(), "a Pong is not answered");
        // No ping was ever sent, so both samples stay at their initial values:
        // `u64::MAX` is the "no RTT measured yet" sentinel, `0` the "never".
        assert_eq!(state.metrics.last_rtt_ms.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(state.metrics.last_pong_ms.load(Ordering::Relaxed), 0);
    }
}
