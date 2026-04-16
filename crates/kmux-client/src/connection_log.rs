use kmux_protocol::epoch_secs_to_ymd_hms;
use kmux_protocol::messages::PROTOCOL_VERSION;

/// Write a per-connection metadata log entry to the connection log file.
///
/// Called once on first successful authentication. Callers pass the fields
/// that vary between the TUI and GUI apps.
pub fn write_connection_log(
    instance_id: &str,
    client_version: &str,
    server_version: Option<&str>,
    host: &str,
    port: u16,
) {
    let connected_at = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (y, mo, d, h, mi, s) = epoch_secs_to_ymd_hms(secs);
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    };
    let content = format!(
        "instance_id: {instance_id}\n\
         client_version: {client_version}\n\
         server_version: {}\n\
         protocol_version: {PROTOCOL_VERSION}\n\
         destination: {host}:{port}\n\
         transport: QUIC\n\
         connected_at: {connected_at}\n",
        server_version.unwrap_or("unknown"),
    );
    match kmux_protocol::dirs::connection_log_path(instance_id) {
        Ok(path) => {
            if let Err(e) = std::fs::write(&path, &content) {
                tracing::warn!("Failed to write connection log {}: {e}", path.display());
            }
        }
        Err(e) => tracing::warn!("Failed to get connection log path: {e}"),
    }
}
