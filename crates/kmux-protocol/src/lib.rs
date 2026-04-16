pub mod dirs;
pub mod frame;
pub mod messages;

/// QUIC idle timeout in seconds (shared by client and server transport configs).
pub const QUIC_IDLE_TIMEOUT_SECS: u64 = 300;
/// QUIC keep-alive interval in seconds (shared by client and server transport configs).
pub const QUIC_KEEP_ALIVE_SECS: u64 = 15;

pub use frame::{ProtocolError, decode_client, decode_server, encode_client, encode_server};
#[cfg(feature = "framing")]
pub use frame::{read_frame, write_frame};
pub use messages::{
    CellAttrs, CellColor, CellState, ClientId, ClientMessage, CursorShape, CursorState, DiffOp,
    ErrorCode, GridSnapshot, PaneId, PaneInfo, RequestId, ServerMessage, SessionEntry,
    SessionEventMsg, SessionMeta, TermModes, TermSize, TerminalDiff, TransportKind, WordId,
    epoch_secs_to_ymd_hms, version_mismatch_hint,
};
