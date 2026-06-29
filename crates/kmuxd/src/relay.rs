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
use crate::backend::{BackendEventSink, ControlEvent};
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
                    title_sink.on_control_event(ControlEvent::Title(&name));
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

            // Certify the grid as of this viewport seqno, after the data it
            // covers — on the same channel so it can never overtake that data.
            if digest_due(update_seqno.0) {
                broadcast_grid_digest(pane_id, clients, snapshot_fn, update_seqno);
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
                if digest_due(seqno.0) {
                    broadcast_grid_digest(pane_id, clients, snapshot_fn, seqno);
                }
            }
        }
        DiffResult::None => {
            debug!(pane_id, "flush_cell_diff: no changes");
        }
    }
}

/// Default cadence (in seqnos) at which the daemon certifies a pane's grid with
/// a `GridDigest`. Throttled because each digest recomputes a full grid hash;
/// 1-in-N keeps the hot path cheap while still catching drift within a few
/// frames. Overridable via `KMUX_GRID_DIGEST_INTERVAL` (1 = every frame, used by
/// the conformance/e2e tests).
const DIGEST_SEQNO_INTERVAL: u64 = 32;

/// Whether a `GridDigest` should be emitted for this seqno.
fn digest_due(seqno: u64) -> bool {
    static INTERVAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let interval = *INTERVAL.get_or_init(|| {
        std::env::var("KMUX_GRID_DIGEST_INTERVAL")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(DIGEST_SEQNO_INTERVAL)
    });
    seqno.is_multiple_of(interval)
}

/// Emit a `GridDigest` certifying the authoritative grid as of `seqno` to every
/// diff-reconstructing client.
///
/// Best-effort by design: a client whose data channel is momentarily full just
/// misses this check (the next digest re-certifies, and a real frame will mark
/// it `Lagged` if it is truly behind), so — unlike `broadcast_to_clients` — we
/// never mark a client lagged or dead for a digest. Force-full-snapshot clients
/// are skipped: they receive whole snapshots and cannot desync.
fn broadcast_grid_digest(
    pane_id: &str,
    clients: &ClientMap,
    snapshot_fn: &dyn Fn() -> GridSnapshot,
    seqno: SequenceNo,
) {
    let hash = snapshot_fn().live_digest();
    let msg = ServerMessage::GridDigest {
        pane_id: pane_id.to_string(),
        seqno,
        hash,
    };
    let map = clients.lock().unwrap();
    for sender in map.values() {
        if sender.output_paused() || sender.force_full_snapshot {
            continue;
        }
        let _ = sender.data_tx.try_send(msg.clone());
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
            // they catch up on resume via re-attach reconciliation. A pane the
            // client marked `no_auto_pause` keeps streaming through an auto-pause.
            if sender.output_paused() {
                continue;
            }
            let outgoing = if sender.force_full_snapshot {
                snapshot_msg.get_or_insert_with(|| {
                    let snapshot = Arc::new(snapshot_fn());
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
                pause_auto: false,
                no_auto_pause: false,
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
                pause_auto: false,
                no_auto_pause: false,
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
                pause_auto: false,
                no_auto_pause: false,
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

    /// End-to-end through the *real* broadcast path: drive `dispatch_diff_result`
    /// for a scripted byte stream with a digest forced after each frame, then
    /// reconstruct the screen on a `CellGrid` from exactly the messages a client
    /// receives. Every emitted `GridDigest` must match the reconstructed grid,
    /// and the final grid must equal the server's authoritative snapshot. This
    /// exercises what the Stage 1 conformance test cannot: real seqno allocation,
    /// `TerminalUpdate`/`ScrollbackAppend`/`CursorUpdate` ordering (including the
    /// reset-first rule), and the wire-level digest agreement.
    #[test]
    fn relay_broadcast_reconstructs_and_digests_match() {
        use kmux_client::grid::CellGrid;

        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(8192);
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: false,
                paused: false,
                pause_auto: false,
                no_auto_pause: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(256 * 1024)));
        let seqno_counter = Arc::new(AtomicU64::new(0));
        let mut prev_cursor = CursorState::default();
        let mut prev_modes = TermModes::EMPTY;
        let snapshot_fn = || ts.lock().unwrap().snapshot();

        // Seed the client exactly as a fresh attach would.
        let mut grid = CellGrid::new(24, 80);
        grid.apply_snapshot(snapshot_fn());

        let script: &[&[u8]] = &[
            b"hello \x1b[31mworld\x1b[0m",
            b"\r\nsecond line with a space",
            b"\x1b[2J\x1b[Hcleared",
            // Overflow the viewport to exercise ScrollbackAppend ordering.
            b"\r\nA\r\nB\r\nC\r\nD\r\nE\r\nF\r\nG\r\nH\r\nI\r\nJ\r\nK\r\nL\r\nM\r\nN\r\nO\r\nP\r\nQ\r\nR\r\nS\r\nT\r\nU\r\nV\r\nW\r\nX\r\nY\r\nZ\r\n",
            b"\x1bcfresh after reset",
        ];
        for chunk in script {
            ts.lock().unwrap().feed(chunk);
            let result = ts.lock().unwrap().compute_diff();
            dispatch_diff_result(
                "eagle/0",
                result,
                0,
                &scrollback,
                &clients,
                &seqno_counter,
                &mut prev_cursor,
                &mut prev_modes,
                &snapshot_fn,
            );
            // Force a digest certifying the latest emitted seqno (bypassing the
            // production throttle so every frame is checked).
            let top = seqno_counter.load(Ordering::Relaxed);
            if top > 0 {
                broadcast_grid_digest("eagle/0", &clients, &snapshot_fn, SequenceNo(top - 1));
            }
        }

        // Replay the client's inbound stream in order, asserting digests as they
        // arrive (the grid is at the certified seqno by construction here).
        let mut digests_checked = 0;
        while let Ok(msg) = data_rx.try_recv() {
            match msg {
                ServerMessage::TerminalUpdate { diff, .. } => {
                    grid.apply_diff((*diff).clone());
                }
                ServerMessage::ScrollbackAppend {
                    first_index, lines, ..
                } => grid.apply_scrollback_append(first_index, lines),
                ServerMessage::CursorUpdate { cursor, modes, .. } => {
                    grid.apply_cursor_update(cursor, modes)
                }
                ServerMessage::GridDigest { hash, .. } => {
                    assert_eq!(
                        grid.live_digest(),
                        hash,
                        "reconstructed grid diverged from the daemon's certified digest"
                    );
                    digests_checked += 1;
                }
                other => panic!("unexpected broadcast message: {other:?}"),
            }
        }

        assert!(digests_checked > 0, "the run must emit and check digests");
        assert_eq!(
            grid.to_snapshot().digest(),
            snapshot_fn().digest(),
            "final reconstructed grid must equal the authoritative snapshot"
        );
    }

    /// Issue #182, §5: force a per-client data-channel overflow → `Lagged`, then
    /// drive the client's resync and assert the grid-digest oracle stays clean
    /// across the recovery. A capacity-4 data channel that is never drained fills
    /// after a few frames; `broadcast_to_clients` then surfaces a `Lagged` (the
    /// issue #68 slow-client path) and drops the client. The client recovers
    /// exactly as the session manager does — clear, re-attach from a fresh
    /// snapshot — and the reconstructed grid must still match the authoritative
    /// one, with every post-resync `GridDigest` agreeing.
    #[test]
    fn oracle_survives_data_channel_overflow_lagged() {
        use kmux_client::grid::CellGrid;

        let make_sender = |data_tx: mpsc::Sender<ServerMessage>| ClientSender {
            data_tx,
            ctrl_tx: mpsc::unbounded_channel().0, // replaced below per client
            force_full_snapshot: false,
            paused: false,
            pause_auto: false,
            no_auto_pause: false,
            capabilities: Default::default(),
            size: Default::default(),
        };
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();

        // A tiny, never-drained data channel overflows after a few frames.
        let (data_tx, _data_rx_full) = mpsc::channel::<ServerMessage>(4);
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                ctrl_tx: ctrl_tx.clone(),
                ..make_sender(data_tx)
            },
        );

        let ts = test_term_state();
        let scrollback = Arc::new(Mutex::new(DiffBuffer::new(256 * 1024)));
        let seqno_counter = Arc::new(AtomicU64::new(0));
        let mut prev_cursor = CursorState::default();
        let mut prev_modes = TermModes::EMPTY;
        let snapshot_fn = || ts.lock().unwrap().snapshot();

        let mut grid = CellGrid::new(24, 80);
        grid.apply_snapshot(snapshot_fn());

        // Flood without draining: the channel fills and overflows to Lagged.
        for i in 0..20 {
            ts.lock().unwrap().feed(format!("line {i}\r\n").as_bytes());
            let result = ts.lock().unwrap().compute_diff();
            dispatch_diff_result(
                "eagle/0",
                result,
                0,
                &scrollback,
                &clients,
                &seqno_counter,
                &mut prev_cursor,
                &mut prev_modes,
                &snapshot_fn,
            );
        }

        let lagged = std::iter::from_fn(|| ctrl_rx.try_recv().ok())
            .any(|m| matches!(m, ServerMessage::Lagged { .. }));
        assert!(
            lagged,
            "an overflowed data channel must surface a Lagged frame"
        );
        assert!(
            clients.lock().unwrap().is_empty(),
            "the lagged client is dropped from the fan-out"
        );

        // Resync: re-attach from a fresh snapshot with a drained channel, exactly
        // as the client's session manager does on Lagged.
        grid.clear();
        grid.apply_snapshot(snapshot_fn());
        let (data_tx2, mut data_rx2) = mpsc::channel::<ServerMessage>(8192);
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                ctrl_tx,
                ..make_sender(data_tx2)
            },
        );

        // Drive more output (with a digest forced after each frame), then replay.
        for i in 20..40 {
            ts.lock().unwrap().feed(format!("more {i}\r\n").as_bytes());
            let result = ts.lock().unwrap().compute_diff();
            dispatch_diff_result(
                "eagle/0",
                result,
                0,
                &scrollback,
                &clients,
                &seqno_counter,
                &mut prev_cursor,
                &mut prev_modes,
                &snapshot_fn,
            );
            let top = seqno_counter.load(Ordering::Relaxed);
            if top > 0 {
                broadcast_grid_digest("eagle/0", &clients, &snapshot_fn, SequenceNo(top - 1));
            }
        }

        let mut digests_checked = 0;
        while let Ok(msg) = data_rx2.try_recv() {
            match msg {
                ServerMessage::TerminalUpdate { diff, .. } => grid.apply_diff((*diff).clone()),
                ServerMessage::ScrollbackAppend {
                    first_index, lines, ..
                } => grid.apply_scrollback_append(first_index, lines),
                ServerMessage::CursorUpdate { cursor, modes, .. } => {
                    grid.apply_cursor_update(cursor, modes)
                }
                ServerMessage::GridDigest { hash, .. } => {
                    assert_eq!(
                        grid.live_digest(),
                        hash,
                        "post-resync grid diverged from the certified digest"
                    );
                    digests_checked += 1;
                }
                other => panic!("unexpected broadcast message: {other:?}"),
            }
        }

        assert!(
            digests_checked > 0,
            "the post-resync run must emit and check digests"
        );
        assert_eq!(
            grid.to_snapshot().digest(),
            snapshot_fn().digest(),
            "grid recovered from Lagged must equal the authoritative snapshot"
        );
    }

    #[test]
    fn grid_digest_delivered_with_authoritative_hash() {
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
                pause_auto: false,
                no_auto_pause: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        let expected = ts.lock().unwrap().snapshot().live_digest();
        broadcast_grid_digest(
            "eagle/0",
            &clients,
            &|| ts.lock().unwrap().snapshot(),
            SequenceNo(7),
        );

        match data_rx
            .try_recv()
            .expect("digest delivered on data channel")
        {
            ServerMessage::GridDigest {
                pane_id,
                seqno,
                hash,
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(seqno, SequenceNo(7));
                assert_eq!(hash, expected, "hash is the authoritative grid live_digest");
            }
            other => panic!("expected GridDigest, got {other:?}"),
        }
    }

    #[test]
    fn grid_digest_skips_snapshot_mode_client() {
        // Force-full-snapshot clients get whole snapshots and cannot desync, so
        // they must never receive a (meaningless) digest.
        let (data_tx, mut data_rx) = mpsc::channel::<ServerMessage>(16);
        let (ctrl_tx, _ctrl_rx) = mpsc::unbounded_channel::<ServerMessage>();
        let clients: ClientMap = Arc::new(Mutex::new(HashMap::new()));
        clients.lock().unwrap().insert(
            ClientId(1),
            ClientSender {
                data_tx,
                ctrl_tx,
                force_full_snapshot: true,
                paused: false,
                pause_auto: false,
                no_auto_pause: false,
                capabilities: Default::default(),
                size: Default::default(),
            },
        );

        let ts = test_term_state();
        broadcast_grid_digest(
            "eagle/0",
            &clients,
            &|| ts.lock().unwrap().snapshot(),
            SequenceNo(7),
        );

        assert!(
            data_rx.try_recv().is_err(),
            "snapshot-mode client must not receive a digest"
        );
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
                    pause_auto: false,
                    no_auto_pause: false,
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
                    pause_auto: false,
                    no_auto_pause: false,
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
                pause_auto: false,
                no_auto_pause: false,
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
