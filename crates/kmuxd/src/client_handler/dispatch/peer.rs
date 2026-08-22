//! Federation: opening and closing upstream peer daemons (issue #121).

use kmux_protocol::messages::{PeerId, PeerTarget, RequestId, ServerMessage};

use super::super::SharedClientState;

/// Handle [`ClientMessage::OpenPeer`].
pub(super) async fn on_open_peer(
    state: &mut SharedClientState,
    request_id: RequestId,
    target: PeerTarget,
) {
    // Ensure an upstream connection to the remote daemon and surface its
    // sessions locally. With the `federation` feature off, `open_peer`
    // returns a "not supported" error and this becomes a `PeerError`
    // the client can surface.
    let peer_hint = target.peer_id();
    match state.app.open_peer(target).await {
        Ok(peer) => state.send(ServerMessage::PeerOpened { request_id, peer }),
        Err(reason) => state.send(ServerMessage::PeerError {
            request_id,
            peer: Some(peer_hint),
            reason,
        }),
    }
}

/// Handle [`ClientMessage::ClosePeer`].
pub(super) fn on_close_peer(state: &mut SharedClientState, request_id: RequestId, peer: PeerId) {
    state.app.close_peer(&peer);
    state.send(ServerMessage::PeerClosed { request_id, peer });
}
