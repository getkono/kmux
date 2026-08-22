//! Federation: opening and closing upstream peer daemons (issue #121).

use kmux_protocol::messages::{PeerId, PeerTarget, RequestId, ServerMessage};

use super::super::SharedClientState;

/// Handle [`ClientMessage::OpenPeer`](kmux_protocol::messages::ClientMessage::OpenPeer).
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

/// Handle [`ClientMessage::ClosePeer`](kmux_protocol::messages::ClientMessage::ClosePeer).
pub(super) fn on_close_peer(state: &mut SharedClientState, request_id: RequestId, peer: PeerId) {
    state.app.close_peer(&peer);
    state.send(ServerMessage::PeerClosed { request_id, peer });
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    #[tokio::test]
    async fn open_peer_that_cannot_be_reached_answers_peer_error_naming_the_peer() {
        // Port 1 on loopback refuses immediately, so this exercises the failure
        // branch without a live peer daemon.
        let (keep, msgs) = dispatch_one(ClientMessage::OpenPeer {
            request_id: 16,
            target: PeerTarget::Direct {
                host: "127.0.0.1".to_string(),
                port: 1,
                token: "tok".to_string(),
                accept_invalid_certs: true,
            },
        })
        .await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::PeerError {
                request_id,
                peer,
                reason,
            } => {
                assert_eq!(request_id, 16);
                assert_eq!(peer.as_deref(), Some("127.0.0.1:1"));
                assert!(
                    reason.starts_with("peer connect failed:"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected PeerError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_peer_that_was_never_opened_reports_success() {
        let (keep, msgs) = dispatch_one(ClientMessage::ClosePeer {
            request_id: 17,
            peer: "nosuchpeer".to_string(),
        })
        .await;
        assert!(keep);
        // Closing is idempotent: an unknown peer is acknowledged, not refused.
        match only(msgs) {
            ServerMessage::PeerClosed { request_id, peer } => {
                assert_eq!(request_id, 17);
                assert_eq!(peer, "nosuchpeer");
            }
            other => panic!("expected PeerClosed, got {other:?}"),
        }
    }
}
