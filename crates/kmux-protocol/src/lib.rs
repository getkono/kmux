pub mod auth;
pub mod codec;
pub mod dirs;
pub mod endpoint;
pub mod messages;
pub mod transport;

#[cfg(feature = "tls")]
pub mod tls;

// QUIC transport constants — re-exported from transport::quic for backward compat.
pub use transport::quic::{QUIC_IDLE_TIMEOUT_SECS, QUIC_KEEP_ALIVE_SECS};

pub use codec::{ProtocolError, decode_client, decode_server, encode_client, encode_server};
#[cfg(feature = "framing")]
pub use codec::{read_frame, write_frame};
pub use endpoint::Endpoint;
pub use messages::{
    CellAttrs, CellColor, CellState, ClientId, ClientMessage, CursorShape, CursorState, DiffOp,
    ErrorCode, GridSnapshot, PaneId, PaneInfo, RequestId, ServerMessage, SessionEntry,
    SessionEventMsg, SessionMeta, TermModes, TermSize, TerminalDiff, TransportKind, WordId,
    epoch_secs_to_ymd_hms, version_mismatch_hint,
};
#[cfg(feature = "framing")]
pub use transport::bootstrap::{Bootstrap, BootstrapError, EndpointAdvert, SessionContext};
