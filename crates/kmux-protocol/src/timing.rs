//! The data plane's timing: every interval, deadline and timeout the client,
//! the daemon and a federation hub must agree on, in one place.
//!
//! `docs/protocol.md` ("Timing") names each of these and what it bounds; its
//! table is checked against this module by the spec conformance test, so
//! neither can drift. Timings that are one process's own business — a
//! redraw tick, a supervisor's restart backoff — stay beside their code.
//!
//! Three relations hold between them, and are checked at compile time below:
//! a peer is declared silent only after several pings could have been
//! answered, the daemon waits longer for a pong than a client waits for any
//! frame (a client stuck behind a slow link is closed by the client first),
//! and a stalled pane stream is reset only after QUIC itself would have
//! called the connection idle.

use std::time::Duration;

// ── Liveness ────────────────────────────────────────────────────────────────

/// How often each end of a link pings the other: the daemon its client, a
/// client its daemon, a federation hub its peer. A ping is answered with a
/// `Pong` carrying the same `seq`.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long a client (a GUI, or a federation hub on its upstream link) lets
/// its daemon stay silent — not a frame, not a pong — before it calls the
/// link dead: three missed pings.
pub const SILENCE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the daemon waits for the `Pong` to a ping it put on the wire
/// before it closes the connection.
pub const PONG_DEADLINE: Duration = Duration::from_secs(30);

// ── Handshake ───────────────────────────────────────────────────────────────

/// How long the daemon gives a new connection to finish authenticating
/// (`Auth` and `AuthProof`) before it closes it, sending nothing.
pub const AUTH_DEADLINE: Duration = Duration::from_secs(30);

/// How long a client waits for the `AuthResult` after sending `Auth`, before
/// it gives the connection up. A federation hub waits this long for each
/// handshake message in turn, so its worst case is twice this.
pub const AUTH_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the daemon waits, after queueing a refused `AuthResult`, for its
/// writer to put it on the wire before closing the connection.
pub const AUTH_REFUSAL_FLUSH: Duration = Duration::from_secs(1);

/// The longest a TLS or QUIC handshake may take on the daemon's listener
/// before the attempt is dropped.
pub const TRANSPORT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

// ── Writes ──────────────────────────────────────────────────────────────────

/// The longest one frame write, or a flush, may take before the daemon
/// closes the connection: a peer that stops reading would otherwise pin its
/// writer forever.
pub const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// QUIC's idle timeout, set identically on both ends.
pub const QUIC_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// How often QUIC sends a keep-alive on an otherwise idle connection.
pub const QUIC_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// How long one QUIC pane stream may stay blocked before the daemon resets
/// that stream alone and sends the client `Lagged` for the pane.
pub const PANE_STREAM_STALL_TIMEOUT: Duration =
    Duration::from_secs(QUIC_IDLE_TIMEOUT.as_secs() + 30);

// ── Federation ──────────────────────────────────────────────────────────────

/// The longest one attempt to open or re-open a link to a federated peer may
/// take, end to end.
pub const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a hub waits for its peer's `SessionListResult` or
/// `ClientListResult`.
pub const PEER_LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a hub waits for its peer to confirm a request it forwarded: a
/// session created, closed or a tab closed, a client kicked.
pub const PEER_CREATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a hub waits for its peer's process overview before answering
/// with its own panes only.
pub const PEER_OVERVIEW_TIMEOUT: Duration = Duration::from_secs(2);

// ── Reconnect ───────────────────────────────────────────────────────────────

/// The first delay before re-opening a dropped link (a GUI's, or a hub's to
/// its peer). Each further attempt doubles it, up to [`BACKOFF_MAX`].
pub const BACKOFF_MIN: Duration = Duration::from_millis(250);

/// The longest delay between two attempts to re-open a link.
pub const BACKOFF_MAX: Duration = Duration::from_secs(15);

/// The most jitter takes off a delay, in permille of it: links that dropped
/// together do not retry together.
pub const BACKOFF_JITTER_CAP_PERMILLE: u32 = 200;

const _: () = {
    assert!(SILENCE_TIMEOUT.as_secs() >= 2 * PING_INTERVAL.as_secs());
    assert!(PONG_DEADLINE.as_secs() > SILENCE_TIMEOUT.as_secs());
    assert!(PANE_STREAM_STALL_TIMEOUT.as_secs() > QUIC_IDLE_TIMEOUT.as_secs());
    assert!(QUIC_KEEP_ALIVE.as_secs() < QUIC_IDLE_TIMEOUT.as_secs());
    assert!(BACKOFF_MIN.as_millis() < BACKOFF_MAX.as_millis());
    assert!(BACKOFF_JITTER_CAP_PERMILLE < 1000);
};
