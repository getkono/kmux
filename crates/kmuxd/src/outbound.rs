//! A connection's outbound message queue, and the signal that closes the
//! connection (issue #206).
//!
//! Every message the daemon sends a TCP/UDS client — replies, events, pings,
//! and the pane data a [`TcpAttacher`](crate::tcp_listener::TcpAttacher)
//! forwards — goes through one FIFO the connection's writer task drains. It
//! used to be unbounded, so a client on a slow link grew the daemon's memory
//! for as long as it stayed connected. It is bounded now, with two lanes over
//! the one queue, so order is kept and memory is not:
//!
//! - **Control** ([`OutboundTx::send`]) is never dropped. It may use the whole
//!   queue. If even that is full, the client has stopped reading, and the
//!   connection is closed ([`CloseReason::OutboundOverflow`]) rather than a
//!   message silently lost; the client reconnects and resyncs.
//! - **Pane data** ([`OutboundTx::try_send_data`]) may use all but a quarter
//!   of the queue, which stays free for control. When it cannot, the pane stream is lagged: its
//!   frames are dropped until the queue has drained to half, then the stream
//!   is resynced with `SyncReset` + `TerminalSnapshot` — the shape of
//!   `AttachResult::SyncReset` — so the client ends up with a correct grid
//!   without a round trip. See [`forward_pane_stream`].

use std::future::Future;
use std::sync::Arc;

use kmux_protocol::messages::{GridSnapshot, SequenceNo, ServerMessage, epoch_millis};
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::app::AttachResult;
use crate::client_handler::build_attach_replay;

/// Messages one connection may have queued for its writer.
///
/// Sized so a healthy client never gets near it: the writer drains up to
/// `MAX_WRITE_BATCH` per flush, and pane data is capped well below it.
pub const OUTBOUND_CAPACITY: usize = 1024;

/// The share of a queue pane data may never take (one slot in
/// `CONTROL_RESERVE_DIVISOR`), kept for control messages so a pane flood
/// cannot crowd out a reply, an event or a ping: 256 of the default 1024.
const CONTROL_RESERVE_DIVISOR: usize = 4;

/// Why the daemon closed a connection. Logged when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// A control message found the outbound queue full: the client stopped
    /// reading.
    OutboundOverflow,
    /// A frame write or flush did not finish within the write timeout.
    WriteTimeout,
    /// A frame write or flush failed: the transport is gone.
    WriteFailed,
    /// The connection did not authenticate within the auth deadline.
    AuthDeadline,
    /// No frame arrived within the pong deadline of an unanswered ping.
    PongDeadline,
}

/// Closes a connection. Cloned into everything that can decide to: the
/// outbound queue, the writer task and the liveness watchdog. The first reason
/// wins; later ones are ignored.
#[derive(Clone)]
pub struct Closer(Arc<watch::Sender<Option<CloseReason>>>);

/// What the connection's read loop waits on to learn it must close.
pub struct CloseSignal(watch::Receiver<Option<CloseReason>>);

/// A connected [`Closer`] / [`CloseSignal`] pair.
pub fn close_channel() -> (Closer, CloseSignal) {
    let (tx, rx) = watch::channel(None);
    (Closer(Arc::new(tx)), CloseSignal(rx))
}

impl Closer {
    /// Ask the connection to close for `reason`, unless it already is.
    pub fn close(&self, reason: CloseReason) {
        self.0.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }
}

impl CloseSignal {
    /// Resolve once the connection has been asked to close, with the reason.
    pub async fn closed(&mut self) -> CloseReason {
        let reason = self
            .0
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|reason| *reason);
        match reason {
            Some(reason) => reason,
            // Every `Closer` is gone, so nothing can close the connection
            // any more: never resolve.
            None => std::future::pending().await,
        }
    }
}

/// The queue is gone: the connection's writer has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundClosed;

/// Why a pane-data message was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRejected {
    /// Queuing it would eat into the slots kept for control.
    Congested,
    /// The writer has stopped.
    Closed,
}

/// The sending half of a connection's outbound queue. Cheap to clone; every
/// producer for the connection holds one.
#[derive(Clone)]
pub struct OutboundTx {
    tx: mpsc::Sender<ServerMessage>,
    closer: Closer,
    /// Bumped by the writer after each batch, so a lagged pane stream can wait
    /// for the queue to drain instead of polling it.
    drained: watch::Receiver<u64>,
    /// Slots pane data leaves free.
    reserve: usize,
    /// Free slots a lagged pane stream waits for before resyncing.
    resume_free: usize,
}

/// The writer task's half of the queue.
pub struct OutboundRx {
    rx: mpsc::Receiver<ServerMessage>,
    drained: watch::Sender<u64>,
}

/// A queue of `capacity` messages whose control overflow closes the
/// connection through `closer`. Pane data leaves a quarter of it for control.
pub fn channel(capacity: usize, closer: Closer) -> (OutboundTx, OutboundRx) {
    let (tx, rx) = mpsc::channel(capacity);
    let (drained_tx, drained_rx) = watch::channel(0);
    let out = OutboundTx {
        tx,
        closer,
        drained: drained_rx,
        reserve: capacity / CONTROL_RESERVE_DIVISOR,
        resume_free: capacity / 2,
    };
    (
        out,
        OutboundRx {
            rx,
            drained: drained_tx,
        },
    )
}

impl OutboundTx {
    /// Queue a control message. Never dropped: if the queue is full the
    /// client has stopped reading, so the connection is closed instead.
    pub fn send(&self, msg: ServerMessage) -> Result<(), OutboundClosed> {
        match self.tx.try_send(msg) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(msg)) => {
                error!(
                    queued = self.len(),
                    category = ?msg.category(),
                    "outbound queue full of unread messages; closing the connection"
                );
                self.closer.close(CloseReason::OutboundOverflow);
                Err(OutboundClosed)
            }
            Err(TrySendError::Closed(_)) => Err(OutboundClosed),
        }
    }

    /// Queue a control message, waiting for room rather than closing the
    /// connection. For bulk replies the client asked for (a log dump) whose
    /// producer can afford to wait. Like pane data it leaves the control
    /// reserve free, so a dump never starves replies, events or pings.
    pub async fn send_waiting(&self, mut msg: ServerMessage) -> Result<(), OutboundClosed> {
        loop {
            self.data_room().await;
            if self.tx.is_closed() {
                return Err(OutboundClosed);
            }
            if self.tx.capacity() > self.reserve {
                match self.tx.try_send(msg) {
                    Ok(()) => return Ok(()),
                    Err(TrySendError::Full(back)) => msg = back,
                    Err(TrySendError::Closed(_)) => return Err(OutboundClosed),
                }
            }
        }
    }

    /// Queue a pane-data message if that leaves the control reserve free.
    pub fn try_send_data(&self, msg: ServerMessage) -> Result<(), DataRejected> {
        if self.tx.capacity() <= self.reserve {
            return Err(if self.tx.is_closed() {
                DataRejected::Closed
            } else {
                DataRejected::Congested
            });
        }
        self.tx.try_send(msg).map_err(|e| match e {
            TrySendError::Full(_) => DataRejected::Congested,
            TrySendError::Closed(_) => DataRejected::Closed,
        })
    }

    /// Wait until the queue has drained to half, or the writer has stopped.
    pub async fn data_room(&self) {
        let mut drained = self.drained.clone();
        loop {
            drained.borrow_and_update();
            if self.tx.capacity() >= self.resume_free || self.tx.is_closed() {
                return;
            }
            if drained.changed().await.is_err() {
                return;
            }
        }
    }

    /// Whether the writer has stopped.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Messages queued and not yet taken by the writer.
    pub fn len(&self) -> usize {
        self.tx.max_capacity() - self.tx.capacity()
    }
}

impl OutboundRx {
    /// The next queued message, or `None` once every sender is gone.
    pub async fn recv(&mut self) -> Option<ServerMessage> {
        self.rx.recv().await
    }

    /// A queued message, if one is ready.
    pub fn try_recv(&mut self) -> Result<ServerMessage, TryRecvError> {
        self.rx.try_recv()
    }

    /// Tell lagged pane streams the writer has taken a batch off the queue.
    pub fn mark_drained(&self) {
        self.drained.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// The seqno of an incremental pane-data frame, which a resync snapshot
/// taken at or after that seqno supersedes. Snapshots are self-contained and
/// never superseded.
fn incremental_seqno(msg: &ServerMessage) -> Option<SequenceNo> {
    match msg {
        ServerMessage::TerminalUpdate { seqno, .. }
        | ServerMessage::ScrollbackAppend { seqno, .. }
        | ServerMessage::CursorUpdate { seqno, .. }
        | ServerMessage::GridDigest { seqno, .. } => Some(*seqno),
        _ => None,
    }
}

/// Whether a pane stream is keeping up with the client.
enum Flow {
    Live,
    /// Frames are being dropped until the queue drains; `dropped` counts them.
    Lagged {
        dropped: u64,
    },
}

/// Stream one attached pane into a connection's outbound queue: the attach
/// replay, then every live frame from `client_rx`, as pane data.
///
/// When the queue is congested the stream goes [`Flow::Lagged`]: frames are
/// dropped (and `client_rx` kept drained, so the relay never marks this
/// client lagged) until [`OutboundTx::data_room`], then `resync` supplies a
/// fresh snapshot and its seqno and the stream sends `SyncReset` +
/// `TerminalSnapshot` and goes live again, skipping incremental frames the
/// snapshot already covers. A pane `resync` cannot snapshot (a federated one)
/// gets a `Lagged` instead, and the client re-attaches.
pub async fn forward_pane_stream<R, F>(
    pane_id: String,
    result: AttachResult,
    mut client_rx: mpsc::Receiver<ServerMessage>,
    out: OutboundTx,
    resync: R,
) where
    R: Fn() -> F,
    F: Future<Output = Option<(GridSnapshot, SequenceNo)>>,
{
    // The attach replay goes first, as pane data like any other frame; a lag
    // discards what is left of it, since the resync snapshot supersedes it.
    let mut replay = build_attach_replay(result, &pane_id).into_iter();
    let mut flow = Flow::Live;
    let mut covered_through: Option<SequenceNo> = None;
    loop {
        flow = match flow {
            Flow::Live => {
                let next = match replay.next() {
                    Some(msg) => Some(msg),
                    None => client_rx.recv().await,
                };
                let Some(msg) = next else {
                    return;
                };
                if let (Some(seqno), Some(covered)) = (incremental_seqno(&msg), covered_through)
                    && seqno <= covered
                {
                    continue;
                }
                match out.try_send_data(msg) {
                    Ok(()) => Flow::Live,
                    Err(DataRejected::Closed) => return,
                    Err(DataRejected::Congested) => {
                        warn!(
                            pane_id,
                            queued = out.len(),
                            "outbound queue congested; pane stream lagged"
                        );
                        Flow::Lagged { dropped: 1 }
                    }
                }
            }
            Flow::Lagged { dropped } => {
                tokio::select! {
                    biased;
                    () = out.data_room() => {
                        // The snapshot supersedes everything already waiting.
                        replay = Vec::new().into_iter();
                        match resync_lagged(&pane_id, &mut client_rx, &out, &resync, dropped).await {
                            Some((flow, covered)) => {
                                covered_through = covered.or(covered_through);
                                flow
                            }
                            None => return,
                        }
                    }
                    msg = client_rx.recv() => match msg {
                        Some(_) => Flow::Lagged { dropped: dropped + 1 },
                        None => return,
                    },
                }
            }
        };
    }
}

/// Recover a lagged pane stream once the queue has room: discard what the lag
/// left waiting, then send `SyncReset` + a fresh `TerminalSnapshot`. Returns
/// the flow to continue with and, on success, the seqno the snapshot covers;
/// `None` when the stream is over (the connection closed, the pane is gone,
/// or it cannot be snapshotted and was sent `Lagged` instead).
async fn resync_lagged<R, F>(
    pane_id: &str,
    client_rx: &mut mpsc::Receiver<ServerMessage>,
    out: &OutboundTx,
    resync: &R,
    mut dropped: u64,
) -> Option<(Flow, Option<SequenceNo>)>
where
    R: Fn() -> F,
    F: Future<Output = Option<(GridSnapshot, SequenceNo)>>,
{
    loop {
        match client_rx.try_recv() {
            Ok(_) => dropped += 1,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => return None,
        }
    }
    let Some((snapshot, seqno)) = resync().await else {
        let _ = out.send(ServerMessage::Lagged {
            pane_id: pane_id.to_string(),
            missed_count: dropped,
        });
        return None;
    };
    let reset = ServerMessage::SyncReset {
        pane_id: pane_id.to_string(),
    };
    let snapshot = ServerMessage::TerminalSnapshot {
        pane_id: pane_id.to_string(),
        snapshot: Arc::new(snapshot),
        seqno,
        sent_at_ms: epoch_millis(),
    };
    match out
        .try_send_data(reset)
        .and_then(|()| out.try_send_data(snapshot))
    {
        Ok(()) => {
            info!(pane_id, dropped, "pane stream resynced after lag");
            Some((Flow::Live, Some(seqno)))
        }
        Err(DataRejected::Closed) => None,
        // Refilled meanwhile: a later resync starts over with another
        // `SyncReset`. Yield first, so a queue that stays full cannot spin
        // the stream's task.
        Err(DataRejected::Congested) => {
            tokio::task::yield_now().await;
            Some((Flow::Lagged { dropped }, None))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kmux_protocol::messages::{CursorState, TermModes, TerminalDiff};

    use super::*;
    use crate::fixtures::sample_grid;

    const PANE: &str = "eagle/0";
    /// A bound on waits for the forwarding task, which runs without sleeps.
    const WAIT: Duration = Duration::from_secs(10);

    fn update(seqno: u64) -> ServerMessage {
        ServerMessage::TerminalUpdate {
            pane_id: PANE.to_string(),
            diff: Arc::new(TerminalDiff {
                ops: vec![],
                cursor: CursorState::default(),
                modes: TermModes::EMPTY,
                history_total: 0,
                scrollback_reset: None,
            }),
            seqno: SequenceNo(seqno),
            sent_at_ms: 0,
        }
    }

    async fn next(rx: &mut OutboundRx) -> ServerMessage {
        tokio::time::timeout(WAIT, rx.recv())
            .await
            .expect("a message within the bound")
            .expect("the queue is open")
    }

    /// Yield until `cond` holds, so a spawned task can run to where it waits.
    async fn until(cond: impl Fn() -> bool) {
        tokio::time::timeout(WAIT, async {
            while !cond() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("condition within the bound");
    }

    /// Queue `count` updates (seqnos 1..=count) on a pane stream, as the relay
    /// would, and start forwarding `replay` and then them into `out`.
    fn forward(
        replay: AttachResult,
        count: u64,
        out: &OutboundTx,
        resync: Option<(GridSnapshot, SequenceNo)>,
    ) -> mpsc::Sender<ServerMessage> {
        let (client_tx, client_rx) = mpsc::channel(512);
        for seqno in 1..=count {
            client_tx.try_send(update(seqno)).unwrap();
        }
        tokio::spawn(forward_pane_stream(
            PANE.to_string(),
            replay,
            client_rx,
            out.clone(),
            move || std::future::ready(resync.clone()),
        ));
        client_tx
    }

    /// A client that stops reading: pane data stops at the control reserve
    /// however much the pane produces, control still fits, and once the
    /// client reads again the stream resyncs with `SyncReset` + snapshot and
    /// drops frames the snapshot covers.
    #[tokio::test]
    async fn a_congested_pane_stream_stays_bounded_and_resyncs_once_the_client_reads() {
        let (out, mut rx) = channel(16, close_channel().0);
        let client_tx = forward(
            AttachResult::Delta(vec![]),
            50,
            &out,
            Some((sample_grid(), SequenceNo(60))),
        );

        // 16 slots, 4 kept for control: 12 updates fit, the other 38 are
        // dropped (the stream is drained into the lag, not left to back up).
        until(|| client_tx.capacity() == 512).await;
        assert_eq!(out.len(), 12, "pane data never takes the control reserve");
        out.send(ServerMessage::Pong { seq: 7 })
            .expect("control fits while pane data is congested");

        // The client reads again, as the writer task would.
        for seqno in 1..=12 {
            let msg = next(&mut rx).await;
            assert!(
                matches!(&msg, ServerMessage::TerminalUpdate { seqno: s, .. } if *s == SequenceNo(seqno)),
                "expected update {seqno}, got {msg:?}"
            );
        }
        assert!(matches!(
            next(&mut rx).await,
            ServerMessage::Pong { seq: 7 }
        ));
        rx.mark_drained();

        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg, ServerMessage::SyncReset { pane_id } if pane_id == PANE),
            "expected SyncReset, got {msg:?}"
        );
        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg, ServerMessage::TerminalSnapshot { seqno, .. } if *seqno == SequenceNo(60)),
            "expected the resync snapshot, got {msg:?}"
        );

        // Live again: a frame the snapshot covers is skipped, a later one sent.
        client_tx.send(update(55)).await.unwrap();
        client_tx.send(update(61)).await.unwrap();
        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg, ServerMessage::TerminalUpdate { seqno, .. } if *seqno == SequenceNo(61)),
            "expected update 61, got {msg:?}"
        );
    }

    /// The attach replay is pane data too: it goes out first and counts
    /// toward the bound. A pane the daemon cannot snapshot (a federated one)
    /// then falls back to `Lagged`, counting what it dropped, so the client
    /// re-attaches.
    #[tokio::test]
    async fn a_lagged_stream_without_a_snapshot_tells_the_client_it_lagged() {
        let (out, mut rx) = channel(16, close_channel().0);
        let replay = AttachResult::FullSnapshot(sample_grid(), SequenceNo(0));
        let client_tx = forward(replay, 20, &out, None);
        until(|| client_tx.capacity() == 512).await;

        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg, ServerMessage::TerminalSnapshot { seqno, .. } if *seqno == SequenceNo(0)),
            "the replay goes first, got {msg:?}"
        );
        for seqno in 1..=11 {
            let msg = next(&mut rx).await;
            assert!(
                matches!(&msg, ServerMessage::TerminalUpdate { seqno: s, .. } if *s == SequenceNo(seqno)),
                "expected update {seqno}, got {msg:?}"
            );
        }
        rx.mark_drained();
        let msg = next(&mut rx).await;
        assert!(
            matches!(&msg, ServerMessage::Lagged { pane_id, missed_count: 9 } if pane_id == PANE),
            "expected Lagged with 9 missed, got {msg:?}"
        );
    }

    /// A stream whose connection's writer has gone stops instead of waiting
    /// for room that will never come.
    #[tokio::test]
    async fn is_closed_reports_the_writer_gone() {
        let (out, rx) = channel(16, close_channel().0);
        assert!(!out.is_closed());
        drop(rx);
        assert!(out.is_closed());
        assert_eq!(out.try_send_data(update(1)), Err(DataRejected::Closed));
    }

    /// A bulk reply waits for room without taking the control reserve, so a
    /// ping still fits while it waits; it goes out once the writer drains.
    #[tokio::test]
    async fn send_waiting_leaves_the_control_reserve_free() {
        let (out, mut rx) = channel(16, close_channel().0);
        for seqno in 1..=12 {
            out.try_send_data(update(seqno)).unwrap();
        }
        let bulk = out.clone();
        let waiting = tokio::spawn(async move {
            bulk.send_waiting(ServerMessage::LogEnd { request_id: 1 })
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(out.len(), 12, "the bulk reply waits outside the reserve");
        out.send(ServerMessage::Pong { seq: 1 })
            .expect("control still fits");

        for _ in 0..13 {
            next(&mut rx).await;
        }
        rx.mark_drained();
        let sent = tokio::time::timeout(WAIT, waiting).await;
        assert_eq!(sent.expect("sent in time").unwrap(), Ok(()));
        assert!(matches!(
            next(&mut rx).await,
            ServerMessage::LogEnd { request_id: 1 }
        ));
    }

    /// Control is never dropped: when it no longer fits, the connection is
    /// closed instead.
    #[tokio::test]
    async fn a_control_message_on_a_full_queue_closes_the_connection() {
        let (closer, mut signal) = close_channel();
        let (out, _rx) = channel(4, closer);
        for seq in 0..4 {
            out.send(ServerMessage::Pong { seq }).expect("room");
        }
        assert_eq!(
            out.send(ServerMessage::Pong { seq: 4 }),
            Err(OutboundClosed)
        );
        let reason = tokio::time::timeout(WAIT, signal.closed()).await;
        assert_eq!(reason, Ok(CloseReason::OutboundOverflow));
    }
}
