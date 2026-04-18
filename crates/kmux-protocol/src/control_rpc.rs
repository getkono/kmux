use serde::{Deserialize, Serialize};

use crate::messages::SessionMeta;

/// JSON request sent to the daemon control socket.
#[derive(Deserialize)]
pub struct ControlRequest {
    pub command: String,
}

/// JSON response to the `"status"` control command.
#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub port: u16,
    #[serde(default)]
    pub tcp_port: u16,
    pub token: String,
    pub pid: u32,
    #[serde(default)]
    pub uptime_secs: u64,
    #[serde(default)]
    pub session_count: usize,
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default)]
    pub kmuxd_version: String,
    #[serde(default)]
    pub endpoints: Vec<EndpointEntry>,
}

/// An advertised transport endpoint in a `StatusResponse`.
#[derive(Serialize, Deserialize, Clone)]
pub struct EndpointEntry {
    pub kind: String,
    pub address: String,
}

/// JSON response to the `"stop"` control command.
#[derive(Serialize, Deserialize)]
pub struct StopResponse {
    pub status: String,
}

/// JSON response to the `"sessions"` control command.
#[derive(Serialize, Deserialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionConnections>,
    /// Auth'd connections that are not attached to any session pane.
    pub unattached: Vec<ConnectionInfo>,
}

/// A session and the connections attached to any of its panes.
#[derive(Serialize, Deserialize)]
pub struct SessionConnections {
    pub meta: SessionMeta,
    pub panes_count: usize,
    pub connections: Vec<ConnectionInfo>,
}

/// Per-connection telemetry snapshot.
#[derive(Serialize, Deserialize, Clone)]
pub struct ConnectionInfo {
    pub connection_id: u64,
    pub client_id: u64,
    pub transport: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
    /// Seconds since this connection was registered.
    pub uptime_secs: u64,
    /// Milliseconds since any inbound frame (None if no frame ever received).
    #[serde(default)]
    pub last_activity_ago_ms: Option<u64>,
    /// Milliseconds since the last successful ping/pong round-trip (None if never).
    #[serde(default)]
    pub last_pong_ago_ms: Option<u64>,
    /// Most recent ping RTT in milliseconds (None if never measured).
    #[serde(default)]
    pub last_rtt_ms: Option<u64>,
}
