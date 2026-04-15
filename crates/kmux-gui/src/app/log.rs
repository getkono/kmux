use std::time::{SystemTime, UNIX_EPOCH};

use kmux_protocol::messages::PROTOCOL_VERSION;
use tracing::warn;

use super::kmuxApp;

impl kmuxApp {
    /// Write a per-connection metadata log on first successful authentication.
    pub(super) fn write_connection_log(&self) {
        let connected_at = {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let (y, mo, d, h, mi, s) = epoch_secs_to_ymd_hms(secs);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        };
        let content = format!(
            "instance_id: {}\nclient_version: {}\nserver_version: {}\nprotocol_version: {}\ndestination: {}:{}\ntransport: QUIC\nconnected_at: {}\n",
            self.instance_id,
            env!("CARGO_PKG_VERSION"),
            self.mgr.server_version.as_deref().unwrap_or("unknown"),
            PROTOCOL_VERSION,
            self.mgr.host(),
            self.mgr.port(),
            connected_at,
        );
        match kmux_protocol::dirs::connection_log_path(&self.instance_id) {
            Ok(path) => {
                if let Err(e) = std::fs::write(&path, &content) {
                    warn!("Failed to write connection log {}: {e}", path.display());
                }
            }
            Err(e) => warn!("Failed to get connection log path: {e}"),
        }
    }
}

/// Convert Unix timestamp (seconds) to (year, month, day, hour, minute, second) UTC.
pub(super) fn epoch_secs_to_ymd_hms(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let days = secs / 86400;
    let time = secs % 86400;
    let h = (time / 3600) as u32;
    let mi = ((time % 3600) / 60) as u32;
    let s = (time % 60) as u32;
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y } as u32;
    (y, mo, d, h, mi, s)
}
