//! The host-facing half of the protocol stack.
//!
//! This crate was extracted from `kmux-protocol`, which had grown to hold two
//! unrelated jobs: the wire format — message types, codec, compatibility
//! classification — and everything needed to *reach* a peer, which is to say
//! XDG paths, Ed25519 identity, TLS material and four transports. The second
//! set drags in `nix`, `rustls`, `rcgen`, `ring`, `quinn` and `toml`; the first
//! needs none of them, and every crate in the workspace depends on the first.
//!
//! The split runs one way — `kmux-sys` depends on `kmux-protocol`, never the
//! reverse — which is what keeps the wire types testable with values alone.
//! `xtask/tests/dependency_direction.rs` asserts it.

pub mod auth;
pub mod dirs;
pub mod log_tail;
pub mod transport;

#[cfg(feature = "identity")]
pub mod identity;

#[cfg(feature = "tls")]
pub mod tls;

// QUIC transport constants — re-exported for the callers that had them from
// `kmux_protocol::` before the split.
pub use transport::quic::{QUIC_IDLE_TIMEOUT_SECS, QUIC_KEEP_ALIVE_SECS};

pub use transport::EndpointAdvert;
