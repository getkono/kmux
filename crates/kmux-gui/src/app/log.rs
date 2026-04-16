use super::kmuxApp;

impl kmuxApp {
    /// Write a per-connection metadata log on first successful authentication.
    pub(super) fn write_connection_log(&self) {
        kmux_client::connection_log::write_connection_log(
            &self.instance_id,
            env!("CARGO_PKG_VERSION"),
            self.mgr.server_version.as_deref(),
            self.mgr.host(),
            self.mgr.port(),
        );
    }
}
