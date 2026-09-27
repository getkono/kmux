//! The one door to a peer for a client's request (issue #227).
//!
//! What a hub does with each message is declared once, in
//! `ClientMessage::federation` (`docs/protocol.md`, the catalogue's
//! **Federated** column). Every *forwarded* request for a session this hub
//! proxies goes to its peer from here, before the router's `match`, so no
//! handler below has a federated branch of its own; the *aggregated* ones
//! (attach, detach, resize, the input lock) keep theirs, since the hub
//! combines them across its viewers.

use kmux_protocol::messages::{ClientId, ClientMessage};

use crate::app::Requester;

use super::super::SharedClientState;

/// Forward `msg` to its peer when it is a forwarded request for a session
/// this hub proxies, answering a refusal at once; otherwise hand it back for
/// the local handlers.
pub(super) async fn route(
    state: &mut SharedClientState,
    client_id: ClientId,
    mut msg: ClientMessage,
) -> Option<ClientMessage> {
    if !state.app.is_forwarded_request(&mut msg) {
        return Some(msg);
    }
    release_closing_panes(state, client_id, &msg).await;
    let from = Requester::client(client_id, state.ctrl_tx.clone());
    if let Err(refusal) = state.app.forward_to_peer(from, msg) {
        state.send(refusal.into_error());
    }
    None
}

/// A client closing a pane or a session stops viewing what it closes, as it
/// does locally (`on_pane_close`, `on_session_close`), before the close goes
/// to the peer.
async fn release_closing_panes(
    state: &mut SharedClientState,
    client_id: ClientId,
    msg: &ClientMessage,
) {
    let closing: Vec<String> = match msg {
        ClientMessage::PaneClose { pane_id, .. } => vec![pane_id.clone()],
        ClientMessage::SessionClose { word_id, .. } => {
            let prefix = format!("{word_id}/");
            state
                .attached
                .keys()
                .filter(|pane_id| pane_id.starts_with(&prefix))
                .cloned()
                .collect()
        }
        _ => Vec::new(),
    };
    for pane_id in closing {
        if let Some(handle) = state.attached.remove(&pane_id) {
            handle.abort();
            state.app.detach_pane_any(&pane_id, client_id).await;
        }
    }
}

#[cfg(all(test, feature = "federation"))]
mod tests {
    use super::super::testing::*;

    /// Attach `state` to each of `panes` as the router would: a pane stream
    /// handle, and (for a proxied pane) a viewer on the hub.
    fn attach(state: &mut SharedClientState, panes: &[&str]) {
        let client_id = state.client_id.expect("authenticated");
        for pane in panes {
            let (data_tx, _data_rx) = tokio::sync::mpsc::channel(8);
            state.app.federated_attach(
                pane,
                client_id,
                data_tx,
                state.ctrl_tx.clone(),
                TermSize::default(),
            );
            let handle = tokio::spawn(std::future::pending::<()>()).abort_handle();
            state.attached.insert(pane.to_string(), handle);
        }
    }

    /// A request for a proxied session goes to its peer under the peer's ids
    /// and is answered from there, not by the hub (issue #227). A client
    /// closing a pane or a session first stops viewing what it closes, as it
    /// would locally — and no other pane.
    #[tokio::test]
    async fn a_close_for_a_proxied_session_goes_to_its_peer_after_releasing_its_panes() {
        let cases = [
            (
                ClientMessage::PaneClose {
                    request_id: 6,
                    pane_id: "fedlocal/0".to_string(),
                },
                vec!["fedlocal/1", "other/0"],
            ),
            (
                ClientMessage::SessionClose {
                    request_id: 7,
                    word_id: "fedlocal".to_string(),
                },
                vec!["other/0"],
            ),
        ];
        for (close, kept) in cases {
            let (mut state, mut ctrl_rx) = authenticated_client().await;
            let (mut upstream, _peer) = state.app.install_channel_peer("fedlocal", "fedremote");
            attach(&mut state, &["fedlocal/0", "fedlocal/1", "other/0"]);
            while upstream.try_recv().is_ok() {}

            assert!(handle_message(&mut state, close, &NoopAttacher).await);

            let mut left: Vec<&str> = state.attached.keys().map(String::as_str).collect();
            left.sort_unstable();
            assert_eq!(left, kept);
            let sent: Vec<ClientMessage> = std::iter::from_fn(|| upstream.try_recv().ok())
                .filter(|m| !matches!(m, ClientMessage::Ping { .. }))
                .collect();
            let (close, detaches) = sent.split_last().expect("something went up");
            let detached = |pane: &str| {
                detaches
                    .iter()
                    .any(|m| matches!(m, ClientMessage::Detach { pane_id } if pane_id == pane))
            };
            assert!(
                detached("fedremote/0") && detaches.len() == 3 - kept.len(),
                "the closer stops viewing what it closes, first: {sent:?}"
            );
            assert!(
                matches!(close, ClientMessage::PaneClose { pane_id, .. } if pane_id == "fedremote/0")
                    || matches!(close, ClientMessage::SessionClose { word_id, .. } if word_id == "fedremote"),
                "then the close goes up: {sent:?}"
            );
            assert!(
                drain(&mut ctrl_rx).is_empty(),
                "the peer answers, not the hub"
            );
        }
    }
}
