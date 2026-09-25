//! Metrics and the current interaction mode.

use super::*;

/// Client-side performance counters for the HUD ticker / metrics inspector.
/// Mirrors `kmux_client::metrics::MetricsSnapshot` + its `DiagCounters`.
#[derive(uniffi::Record)]
pub struct FfiMetrics {
    pub net_apply_avg_ms: f64,
    pub net_apply_max_ms: f64,
    pub apply_avg_ms: f64,
    pub batch_avg: f64,
    pub last_diff_ops: u64,
    pub last_large_diff_ms: f64,
    pub snapshot_mode: bool,
    pub stale_discards: u64,
    pub seqno_gaps: u64,
    pub lag_events: u64,
    pub resyncs: u64,
    // ── Performance counters (issue #61) ──
    /// Whether the latency + FPS counters are enabled (else hidden + uncomputed).
    pub show_perf_counters: bool,
    /// Network round-trip latency (ms) for the active transport; `None` before
    /// the first ping round-trip.
    pub net_latency_ms: Option<f64>,
    /// Whether the link has gone quiet (>3× the ping interval): show the ★ star.
    pub latency_stale: bool,
    /// Rendering frames per second (actual repaints; idles near 0, peaks ~60).
    pub render_fps: u32,
}

/// Which interaction mode / overlay is active. Carries the text the matching
/// overlay needs (connecting label, disconnect reason); list contents are read
/// via the dedicated getters.
#[derive(uniffi::Enum)]
pub enum FfiMode {
    Normal,
    Locked,
    SessionPicker,
    DirectoryPicker,
    /// Unified session launcher (issue #121); rows via `launch_picker()`.
    LaunchPicker,
    /// Add-a-remote form (issue #121); submit via `submit_add_remote`.
    AddRemote,
    /// New-session-on-a-remote path prompt (issue #121); `peer` is the target,
    /// submit via `submit_remote_new_session`.
    RemoteNewSession {
        peer: String,
    },
    Help,
    /// Process overview main-area view (issue #122); rows via `overview_rows()`.
    ProcessOverview,
    /// Connected-clients main-area view (issue #146); rows via `client_rows()`.
    ConnectedClients,
    ConfirmCloseSession {
        word_id: String,
        name: String,
    },
    Command,
    Connecting {
        label: String,
    },
    Disconnected {
        reason: String,
    },
    Other,
}

pub(crate) fn mode_to_ffi(mode: &Mode) -> FfiMode {
    match mode {
        Mode::Normal => FfiMode::Normal,
        Mode::Locked => FfiMode::Locked,
        Mode::SessionPicker => FfiMode::SessionPicker,
        Mode::DirectoryPicker => FfiMode::DirectoryPicker,
        Mode::LaunchPicker => FfiMode::LaunchPicker,
        Mode::AddRemote => FfiMode::AddRemote,
        Mode::RemoteNewSession { peer } => FfiMode::RemoteNewSession { peer: peer.clone() },
        Mode::ProcessOverview => FfiMode::ProcessOverview,
        Mode::ConnectedClients => FfiMode::ConnectedClients,
        Mode::ConfirmCloseSession { word_id, name } => FfiMode::ConfirmCloseSession {
            word_id: word_id.clone(),
            name: name.clone(),
        },
        Mode::Help => FfiMode::Help,
        Mode::Command(_) => FfiMode::Command,
        Mode::Connecting { target_display } => FfiMode::Connecting {
            label: target_display.clone(),
        },
        Mode::Disconnected { reason } => FfiMode::Disconnected {
            reason: reason.clone(),
        },
        _ => FfiMode::Other,
    }
}
