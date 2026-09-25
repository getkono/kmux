//! Federated peer links (issue #121).

use super::*;

impl SessionManager {
    /// Handle a `PeerOpened` frame.
    pub(super) fn on_peer_opened(peer: PeerId) -> Vec<SessionEvent> {
        vec![SessionEvent::PeerOpened { peer }]
    }

    /// Handle a `PeerError` frame.
    pub(super) fn on_peer_error(peer: Option<PeerId>, reason: String) -> Vec<SessionEvent> {
        vec![SessionEvent::PeerError { peer, reason }]
    }

    /// A close ack needs no app-level reconciliation (the peer's sessions
    /// simply stop appearing in the next `SessionList`).
    pub(super) fn on_peer_closed() -> Vec<SessionEvent> {
        Vec::new()
    }
}
