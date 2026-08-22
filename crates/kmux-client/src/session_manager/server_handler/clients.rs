//! Who else is attached, and being told one of them left (issue #146).

use super::*;

impl SessionManager {
    /// Handle a `ClientListResult` frame.
    pub(super) fn on_client_list_result(
        &mut self,
        word_id: WordId,
        clients: Vec<ClientInfo>,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        self.client_list = clients;
        self.client_list_word = Some(word_id.clone());
        events.push(SessionEvent::ClientListReceived { word_id });
        events
    }

    /// Handle a `ClientKicked` frame.
    pub(super) fn on_client_kicked(
        &mut self,
        word_id: WordId,
        client_id: ClientId,
    ) -> Vec<SessionEvent> {
        let mut events = Vec::new();
        // The daemon acks the kick and pushes no refreshed list, so the
        // connected-clients view would keep the row until its own ~1 Hz
        // poll came round — a visible lag that reads as a failed kick.
        // The ack is authoritative for the session it names.
        if self.client_list_word.as_deref() == Some(word_id.as_str()) {
            self.client_list.retain(|c| c.client_id != client_id);
        }
        events.push(SessionEvent::ClientKicked { word_id, client_id });
        events
    }
}
