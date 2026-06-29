pub mod auth;
pub mod buildinfo;
pub mod codec;
pub mod compat;
pub mod control_rpc;
pub mod dirs;
pub mod endpoint;
pub mod log_tail;
pub mod messages;
pub mod trace;
pub mod transport;

#[cfg(feature = "identity")]
pub mod identity;

#[cfg(feature = "tls")]
pub mod tls;

// QUIC transport constants — re-exported from transport::quic for backward compat.
pub use transport::quic::{QUIC_IDLE_TIMEOUT_SECS, QUIC_KEEP_ALIVE_SECS};

#[cfg(feature = "framing")]
pub use codec::{
    Compressor, flush, read_frame, write_frame, write_frame_compressed, write_frame_compressed_into,
};
pub use codec::{ProtocolError, decode_client, decode_server, encode_client, encode_server};
pub use endpoint::Endpoint;
pub use messages::{
    CellAttrs, CellColor, CellState, ClientId, ClientMessage, CursorShape, CursorState, DiffOp,
    ErrorCode, GridSnapshot, PaneId, PaneInfo, RequestId, ServerMessage, SessionEntry,
    SessionEventMsg, SessionMeta, TermModes, TermSize, TerminalDiff, TransportKind, WordId,
    epoch_secs_to_ymd_hms, format_pane_id, pane_index, pane_word, parse_pane_id,
    version_mismatch_hint,
};
#[cfg(feature = "framing")]
pub use transport::bootstrap::{Bootstrap, BootstrapError, EndpointAdvert, SessionContext};
