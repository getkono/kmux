//! Client-side state & rendering model for kmux frontends.
//!
//! Holds the per-connection client state (session list, per-pane [`grid::CellGrid`]
//! buffers, [`liveness`], [`metrics`]) and the toolkit-agnostic pieces a frontend
//! reads to render. The connection/negotiation *mechanism* (transports, daemon
//! lifecycle) was extracted into `kmux-connect` (issue #121) so it can be shared
//! with `kmuxd`'s federation role; it is re-exported here under the original
//! paths so existing `kmux_client::{...}` consumers — and internal
//! `crate::{pipeline,supervisor,…}` references — keep resolving unchanged.

pub mod connection_log;
pub mod connection_state;
pub mod event_log;
pub mod grid;
pub mod input;
pub mod key;
pub mod liveness;
pub mod metrics;
pub mod session_manager;
pub mod transport;

pub use kmux_connect::{
    connect, daemon, hosts, pipeline, set_frontend_kind, ssh, supervisor, tcp_connect, token,
};

use rand::Rng;

/// Generate a random 4-byte hex instance identifier.
pub fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
