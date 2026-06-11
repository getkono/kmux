//! Diagnostic frame-trace record schemas (issue #72: shell tearing).
//!
//! These are **not** part of the wire protocol — they are the on-disk JSONL
//! schema shared between the daemon (which records every emitted diff) and the
//! `kmux debug tearing` analyzer (which reconstructs logical frames and reports
//! torn frames). They live here because `kmux-protocol` is the only crate that
//! both `kmuxd` and `kmux-app` depend on.
//!
//! Each record is serialized as one JSON object per line. The producing crates
//! own the file I/O (`serde_json` + flock / a guarded handle); this module only
//! defines the shapes so the producer and the analyzer cannot drift.

use serde::{Deserialize, Serialize};

/// Which kind of diff the daemon emitted, for grouping in the analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// A cell diff (`ServerMessage::TerminalUpdate`) — carries `ops` cell ops.
    Update,
    /// A cursor-only update (`ServerMessage::CursorUpdate`).
    Cursor,
    /// A scrollback append (`ServerMessage::ScrollbackAppend`).
    Scrollback,
}

/// One diff the daemon emitted for a pane. Written to the daemon trace file
/// (`dirs::daemon_trace_path()`) when `KMUX_FRAME_TRACE` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonDiffRecord {
    pub pane_id: String,
    pub seqno: u64,
    /// `epoch_millis()` stamped on the outgoing message.
    pub sent_at_ms: u64,
    /// Number of cell ops (0 for cursor-only / scrollback records).
    pub ops: usize,
    pub kind: DiffKind,
}

/// One diff applied during a client pump tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedDiff {
    pub seqno: u64,
    pub sent_at_ms: u64,
    /// Cell ops applied (0 for cursor-only).
    pub ops: usize,
}

/// One client pump tick. Written to the client trace file
/// (`dirs::client_trace_path()`) when `KMUX_FRAME_TRACE` is set. `painted` is
/// true when the tick produced a repaint (`FrontendEffect::NeedsRender`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTickRecord {
    pub tick_id: u64,
    /// `epoch_millis()` at the end of the tick's drain phase.
    pub at_ms: u64,
    pub applied: Vec<AppliedDiff>,
    pub painted: bool,
}
