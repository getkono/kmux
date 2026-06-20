use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kmux_protocol::messages::{
    ClientId, CursorState, GridSnapshot, SequenceNo, ServerMessage, TermModes, TerminalDiff,
    epoch_millis,
};
use kmux_protocol::trace::DiffKind;
use kmux_pty::process::ExitStatus;
use kmux_pty::registry::SessionManager;
use kmux_pty::session::PtyReader;
use tokio::time::MissedTickBehavior;
use tracing::{debug, warn};

use crate::app::ClientMap;
use crate::backend::BackendEventSink;
use crate::diff_engine::DiffResult;
use crate::scrollback::DiffBuffer;
use crate::term_state::TermState;

/// Read PTY output in a loop, feed bytes through server-side VT emulation,
/// and immediately compute + broadcast cell diffs after each read.
///
/// Also polls the foreground process name every 500 ms via `tcgetpgrp` so
/// pane titles update as the user switches between commands, even when the
/// shell does not emit OSC 0/2 sequences.
// Each parameter is a distinct shared handle the loop fans output into
// (emulator, scrollback, client map, seqno, registry); bundling them into a
// struct would only add indirection at the three call sites.
#[allow(clippy::too_many_arguments)]
pub async fn session_diff_loop(
    mut reader: PtyReader,
    pane_id: String,
    title_sink: Arc<dyn BackendEventSink>,
    clients: ClientMap,
    scrollback: Arc<Mutex<DiffBuffer>>,
    term_state: Arc<Mutex<TermState>>,
    seqno_counter: Arc<AtomicU64>,
    manager: Arc<SessionManager>,
) {
    let master_fd = reader.as_raw_fd();
    let mut buf = vec![0u8; 65536];
    let mut prev_cursor = CursorState::default();
    let mut prev_modes = TermModes::EMPTY;
    let mut last_fg_name = String::new();

    let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
    poll_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Skip the first tick (fires immediately at t=0).
    poll_interval.tick().await;

    loop {
        tokio::select! {
            result = reader.read(&mut buf) => {
                match result {
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
            _ = poll_interval.tick() => {
                if let Some(name) = foreground_process_name(master_fd)
                    && name != last_fg_name
                {
                    last_fg_name = name.clone();
                    title_sink.on_title(&name);
                }
            }
        }
    }

    // The read loop ended: the PTY master returned EOF (or errored), which means
    // the child exited. Surface this to attached clients — the only runtime exit
    // signal for a foreground child, and the *sole* one for a foreign child
    // inherited across a handoff (which cannot be `waitpid`-ed).
    //
    // If the session is no longer registered, the pane was explicitly closed
    // (`close_pane` removes it and already emits `Closed`), so we stay silent to
    // avoid a redundant `Exited` after `Closed`. Otherwise the shell exited on
    // its own and we report it.
    if let Ok(session) = manager.get_session(&pane_id).await {
        let status = match tokio::time::timeout(Duration::from_secs(2), session.wait()).await {
            Ok(status) => status,
            Err(_) => ExitStatus::Unknown,
        };
        debug!(pane_id, %status, "pane child exited");
        manager.notify_exited(&pane_id, status);
    }
}

/// Return the name of the foreground process running on the PTY.
///
/// Uses `tcgetpgrp(master_fd)` to find the foreground process group, then
/// reads `/proc/<pgid>/comm` for the command name. Returns `None` when the
/// PTY has no foreground process or the name cannot be read.
fn foreground_process_name(master_fd: RawFd) -> Option<String> {
    // SAFETY: master_fd is a dup'd PTY fd owned by PtyReader for the
    // lifetime of session_diff_loop; BorrowedFd is used only during
    // this synchronous call and not retained.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(master_fd) };
    let pgid = nix::unistd::tcgetpgrp(fd).ok()?.as_raw();
    if pgid <= 0 {
        return None;
    }
    std::fs::read_to_string(format!("/proc/{pgid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_string())
        .filter(|s| !s.is_empty())
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
    // Force-full-snapshot clients are re-seeded straight from the emulator.
    let snapshot_fn = || term_state.lock().unwrap().snapshot();
    dispatch_diff_result(
        pane_id,
        result,
        diff_us,
        scrollback,
        clients,
        seqno_counter,
        prev_cursor,
        prev_modes,
        &snapshot_fn,
    );
}

/// Turn a computed [`DiffResult`] into client broadcasts: stamp seqnos, push to
/// the scrollback replay buffer, and fan out `TerminalUpdate` /
/// `ScrollbackAppend` / `CursorUpdate`. Shared by the in-process relay (above)
/// and the out-of-process worker supervisor, which supplies its own
/// `snapshot_fn` (the daemon-side mirror) so both paths fan out identically.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_diff_result(
    pane_id: &str,
    result: DiffResult,
    diff_us: u128,
    scrollback: &Arc<Mutex<DiffBuffer>>,
    clients: &ClientMap,
    seqno_counter: &Arc<AtomicU64>,
    prev_cursor: &mut CursorState,
    prev_modes: &mut TermModes,
    snapshot_fn: &dyn Fn() -> GridSnapshot,
) {
    match result {
        DiffResult::CellDiff {
            diff,
            scrollback_lines,
        } => {
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

            // Scrollback travels out-of-band as `ScrollbackAppend`, referencing
            // absolute indices derived from `history_total`. In v16 the diff no
            // longer carries the lines inline; clients reconcile any gap via
            // `FetchHistory`.
            //
            // Ordering: normally the append is sent before the viewport diff.
            // On a scrollback-*reset* frame the client must wipe its buffer
            // (via the diff's `scrollback_reset`) BEFORE the surviving lines are
            // appended -- otherwise it appends onto stale scrollback and then
            // wipes them. So the `TerminalUpdate` is emitted first in that case.
            // Seqnos are allocated in emission order to stay monotonic.
            let reset_first = diff.scrollback_reset.is_some();
            let next_seqno = || SequenceNo(seqno_counter.fetch_add(1, Ordering::Relaxed));
            let has_sb = !scrollback_lines.is_empty();
            let (update_seqno, sb_seqno) = match (reset_first, has_sb) {
                (true, true) => {
                    let u = next_seqno();
                    (u, Some(next_seqno()))
                }
                (false, true) => {
                    let s = next_seqno();
                    (next_seqno(), Some(s))
                }
                (_, false) => (next_seqno(), None),
            };

            let sb_msg = sb_seqno.map(|sb_seqno| {
                let first_index = diff
                    .history_total
                    .saturating_sub(scrollback_lines.len() as u64);
                let sent_at = epoch_millis();
                crate::trace::record(pane_id, sb_seqno.0, sent_at, 0, DiffKind::Scrollback);
                ServerMessage::ScrollbackAppend {
                    pane_id: pane_id.to_string(),
                    first_index,
                    lines: scrollback_lines,
                    seqno: sb_seqno,
                    sent_at_ms: sent_at,
                }
            });

            let diff = Arc::new(diff);
            scrollback
                .lock()
                .unwrap()
                .push(update_seqno, Arc::clone(&diff));
            let update_sent_at = epoch_millis();
            crate::trace::record(
                pane_id,
                update_seqno.0,
                update_sent_at,
                ops,
                DiffKind::Update,
            );
            let update_msg = ServerMessage::TerminalUpdate {
                pane_id: pane_id.to_string(),
                diff,
                seqno: update_seqno,
                sent_at_ms: update_sent_at,
            };

            if reset_first {
                broadcast_to_clients(pane_id, &update_msg, clients, snapshot_fn, update_seqno);
                if let (Some(sb_msg), Some(sb_seqno)) = (&sb_msg, sb_seqno) {
                    broadcast_to_clients(pane_id, sb_msg, clients, snapshot_fn, sb_seqno);
                }
            } else {
                if let (Some(sb_msg), Some(sb_seqno)) = (&sb_msg, sb_seqno) {
                    broadcast_to_clients(pane_id, sb_msg, clients, snapshot_fn, sb_seqno);
                }
                broadcast_to_clients(pane_id, &update_msg, clients, snapshot_fn, update_seqno);
            }
        }
        DiffResult::CursorOnly {
            cursor,
            modes,
            history_total,
        } => {
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
                        history_total,
                        scrollback_reset: None,
                    }),
                );
                let sent_at = epoch_millis();
                crate::trace::record(pane_id, seqno.0, sent_at, 0, DiffKind::Cursor);
                let msg = ServerMessage::CursorUpdate {
                    pane_id: pane_id.to_string(),
                    cursor,
                    modes,
                    seqno,
                    sent_at_ms: sent_at,
                };
                broadcast_to_clients(pane_id, &msg, clients, snapshot_fn, seqno);
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
    snapshot_fn: &dyn Fn() -> GridSnapshot,
    seqno: SequenceNo,
) {
    let mut dead: Vec<ClientId> = Vec::new();
    let mut snapshot_msg: Option<ServerMessage> = None;

    {
        let map = clients.lock().unwrap();
        for (&client_id, sender) in map.iter() {
            // Paused clients (issue #68) receive no terminal-output frames. They
            // must NOT be marked lagged or dropped when their channel fills —
            // they catch up on resume via re-attach reconciliation.
            if sender.paused {
                continue;
            }
            let outgoing = if sender.force_full_snapshot {
                snapshot_msg.get_or_insert_with(|| {
                    let snapshot = snapshot_fn();
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
                history_total: 0,
                scrollback_reset: None,
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
                paused: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &|| ts.lock().unwrap().snapshot(),
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
                paused: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &|| ts.lock().unwrap().snapshot(),
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
                paused: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &|| ts.lock().unwrap().snapshot(),
            SequenceNo(1),
        );

        let msg = data_rx
            .try_recv()
            .expect("should receive message on data channel");
        assert!(matches!(msg, ServerMessage::TerminalUpdate { .. }));

        assert_eq!(clients.lock().unwrap().len(), 1);
    }

    #[test]
    fn broadcast_skips_paused_client() {
        // A paused client receives nothing; an active client still does.
        let (paused_tx, mut paused_rx) = mpsc::channel::<ServerMessage>(16);
        let (paused_ctrl_tx, _paused_ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let (active_tx, mut active_rx) = mpsc::channel::<ServerMessage>(16);
        let (active_ctrl_tx, _active_ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = clients.lock().unwrap();
            map.insert(
                ClientId(1),
                ClientSender {
                    data_tx: paused_tx,
                    ctrl_tx: paused_ctrl_tx,
                    force_full_snapshot: false,
                    paused: true,
                    capabilities: Default::default(),
                    size: Default::default(),
                },
            );
            map.insert(
                ClientId(2),
                ClientSender {
                    data_tx: active_tx,
                    ctrl_tx: active_ctrl_tx,
                    force_full_snapshot: false,
                    paused: false,
                    capabilities: Default::default(),
                    size: Default::default(),
                },
            );
        }

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &|| ts.lock().unwrap().snapshot(),
            SequenceNo(1),
        );

        assert!(
            paused_rx.try_recv().is_err(),
            "paused client should receive nothing"
        );
        assert!(
            matches!(
                active_rx.try_recv(),
                Ok(ServerMessage::TerminalUpdate { .. })
            ),
            "active client should receive the diff"
        );
        // Both clients remain registered.
        assert_eq!(clients.lock().unwrap().len(), 2);
    }

    #[test]
    fn paused_client_not_marked_lagged_when_data_full() {
        // Even with a full data channel, a paused client is never marked lagged
        // or removed — it catches up on resume.
        let (data_tx, _data_rx) = mpsc::channel::<ServerMessage>(1);
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        data_tx.try_send(dummy_update("eagle/0")).unwrap(); // fill to capacity

        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(7),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                paused: true,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_to_clients(
            "eagle/0",
            &dummy_update("eagle/0"),
            &clients,
            &|| ts.lock().unwrap().snapshot(),
            SequenceNo(2),
        );

        assert!(
            ctrl_rx.try_recv().is_err(),
            "paused client must not receive a Lagged message"
        );
        assert_eq!(
            clients.lock().unwrap().len(),
            1,
            "paused client must not be removed"
        );
    }
}
