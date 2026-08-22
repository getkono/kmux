//! Connection, session, pane and tab views, plus the process overview.

use super::*;

/// Connection lifecycle state (for the connection badge / disconnect overlay).
#[derive(uniffi::Enum)]
pub enum FfiConnStatus {
    Idle,
    Handshaking,
    Connected,
    Reconnecting,
    Disconnected,
}

impl From<&ConnectionState> for FfiConnStatus {
    fn from(s: &ConnectionState) -> Self {
        match s {
            ConnectionState::Idle => Self::Idle,
            ConnectionState::Handshaking => Self::Handshaking,
            ConnectionState::Connected { .. } => Self::Connected,
            ConnectionState::Reconnecting { .. } => Self::Reconnecting,
            ConnectionState::Disconnected { .. } => Self::Disconnected,
        }
    }
}

/// Connection state + a human-readable badge label.
#[derive(uniffi::Record)]
pub struct FfiConnInfo {
    pub status: FfiConnStatus,
    pub label: String,
    /// Whether the transport is pinned via the override (issue #69). When true,
    /// the protocol indicator renders in an "overridden" style.
    pub transport_overridden: bool,
}

/// Recent round-trip-time summary for the active transport (connection
/// inspector). Mirrors `kmux_app::core::RttInfo`.
#[derive(uniffi::Record)]
pub struct FfiRtt {
    /// EWMA latency in ms, or `None` before the first Ping/Pong.
    pub ewma_ms: Option<f64>,
    pub recent_avg_ms: f64,
    pub recent_max_ms: f64,
    pub samples: u64,
}

/// Per-transport byte/message traffic totals (connection inspector). Mirrors
/// `kmux_app::core::TransportTraffic`.
#[derive(uniffi::Record)]
pub struct FfiTransportTraffic {
    pub label: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub msgs_in: u64,
    pub msgs_out: u64,
}

/// The connection / session / handshake technical details rendered by the
/// connection inspector (issue #60). Mirrors `kmux_app::core::ConnectionInfo`.
#[derive(uniffi::Record)]
pub struct FfiConnectionDetails {
    pub server: String,
    pub is_local: bool,
    pub endpoint: String,
    pub state: String,
    pub connected: bool,
    pub transport: String,
    pub connection_id: Option<u64>,
    pub client_id: Option<u64>,
    pub server_version: Option<String>,
    pub protocol_version: String,
    pub accept_invalid_certs: bool,
    pub rtt: Option<FfiRtt>,
    pub transports: Vec<FfiTransportTraffic>,
}

/// One session in the session list.
#[derive(uniffi::Record)]
pub struct FfiSession {
    pub word_id: String,
    pub name: String,
    pub cwd: String,
    pub active: bool,
    /// The federated peer this session lives on (issue #121), or `None` for a
    /// local session. Lets the sidebar group sessions by machine.
    pub peer: Option<String>,
}

/// One pane (tab) in the active session.
#[derive(uniffi::Record)]
pub struct FfiPane {
    pub id: String,
    /// Display label: the pane title, or `"pane N"` (1-based) when untitled.
    pub label: String,
    pub active: bool,
}

/// Tab label: the pane title, falling back to its 1-based index (mirrors the
/// GTK frontend's `pane_label`).
pub(crate) fn pane_label(index: u32, title: &str) -> String {
    if title.trim().is_empty() {
        format!("pane {}", index + 1)
    } else {
        title.to_string()
    }
}

/// One tab (Session → **Tab** → Pane) of the active session, with the viewed
/// tab flagged. Drives the native tab strip.
#[derive(uniffi::Record)]
pub struct FfiTab {
    pub tab_index: u32,
    pub name: String,
    pub active: bool,
    /// Whether any pane of this tab is currently paused (issue #68); drives the
    /// tab strip's pause marker.
    pub paused: bool,
    /// Whether any pane in this tab has an unread BEL or notification.
    pub needs_attention: bool,
}

/// Tab name, falling back to the focused pane's OSC title, then its 1-based
/// index. An explicit tab rename always wins.
pub(crate) fn tab_label(index: u32, name: &str, pane_title: &str) -> String {
    if name.trim().is_empty() {
        if pane_title.trim().is_empty() {
            format!("{}", index + 1)
        } else {
            pane_title.to_string()
        }
    } else {
        name.to_string()
    }
}

/// What a process-overview row represents (issue #122), driving the Swift
/// view's indent and styling per tier.
#[derive(uniffi::Enum)]
pub enum FfiOverviewKind {
    Session,
    Tab,
    Pane,
    Process,
}

/// One flattened process-overview row (issue #122). Mirrors
/// `kmux_app::core::OverviewRow`; the Swift `ProcessOverviewView` indents by
/// `depth` and right-aligns the CPU/memory/PID columns. Polled via
/// [`KmuxDriver::overview_rows`].
#[derive(uniffi::Record)]
pub struct FfiOverviewRow {
    pub depth: u8,
    pub kind: FfiOverviewKind,
    pub label: String,
    pub detail: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
    /// PID for process rows (and the shell pid for pane rows); `None` otherwise.
    pub pid: Option<i32>,
    /// The federated peer this row belongs to (session rows only).
    pub peer: Option<String>,
}

pub(crate) fn overview_kind_to_ffi(kind: OverviewRowKind) -> FfiOverviewKind {
    match kind {
        OverviewRowKind::Session => FfiOverviewKind::Session,
        OverviewRowKind::Tab => FfiOverviewKind::Tab,
        OverviewRowKind::Pane => FfiOverviewKind::Pane,
        OverviewRowKind::Process => FfiOverviewKind::Process,
    }
}
