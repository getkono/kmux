pub mod connect;
pub mod connection_log;

use rand::RngCore;

/// Generate a random 4-byte hex instance identifier.
pub fn generate_instance_id() -> String {
    let mut bytes = [0u8; 4];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub mod daemon;
pub mod event_log;
pub mod grid;
pub mod hosts;
pub mod input;
pub mod key;
pub mod metrics;
pub mod quic_probe;
pub mod session_manager;
pub mod ssh;
pub mod tcp_connect;
pub mod token;
pub mod transport;
