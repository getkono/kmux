//! Connected-clients projection (issue #146).
//!
//! The connected-clients view is a flat list of the [`ClientInfo`]s attached to
//! the active session, as last returned by the daemon. Unlike the process
//! overview, no joining or aggregation is needed — each row is one client — so
//! this accessor simply surfaces the session manager's cached list to the
//! frontends (the GTK list and, via `kmux-ffi`, the SwiftUI view).

use kmux_protocol::messages::ClientInfo;

use super::AppCore;

impl AppCore {
    /// The client connections attached to the active session (issue #146), as of
    /// the most recent [`kmux_client::session_manager::SessionManager::request_client_list`] reply. Empty until
    /// the first reply or when no session is active.
    pub fn client_rows(&self) -> Vec<ClientInfo> {
        self.mgr.client_list.clone()
    }
}
