//! Session lifecycle: create, close, list, restore, rename.

use kmux_protocol::messages::{
    CAPABILITY_SESSION_CLOSED_PEER, ClientId, ClosedSessionEntry, RequestId, ServerMessage,
    SessionEventMsg, WordId,
};

use crate::connection::classify_error;

use super::super::SharedClientState;
use super::Spawn;

/// Handle [`ClientMessage::SessionCreate`](kmux_protocol::messages::ClientMessage::SessionCreate).
pub(super) async fn on_session_create(
    state: &mut SharedClientState,
    request_id: RequestId,
    name: Option<String>,
    cwd: Option<String>,
    spawn: Spawn,
) {
    let Spawn {
        program,
        args,
        size,
    } = spawn;
    match state
        .app
        .create_session(name, cwd, program, args, size, &state.capabilities)
        .await
    {
        Ok(entry) => {
            let word_id = entry.meta.word_id.clone();
            state.send(ServerMessage::SessionCreated { request_id, entry });
            // The reply reaches the requester alone; every other GUI learns of
            // the new session from this broadcast, as it does for a restore.
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionCreated { word_id });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::SessionClose`](kmux_protocol::messages::ClientMessage::SessionClose).
pub(super) async fn on_session_close(
    state: &mut SharedClientState,
    client_id: ClientId,
    request_id: RequestId,
    word_id: WordId,
) {
    let pane_ids: Vec<String> = state
        .attached
        .keys()
        .filter(|k| k.starts_with(&format!("{word_id}/")))
        .cloned()
        .collect();
    for pane_id in &pane_ids {
        if let Some(handle) = state.attached.remove(pane_id) {
            handle.abort();
        }
        state.app.detach_pane_any(pane_id, client_id).await;
    }
    match state.app.close_session(&word_id).await {
        Ok(exit_code) => {
            state.send(ServerMessage::SessionClosed {
                request_id,
                word_id: word_id.clone(),
                exit_code,
            });
            // Everyone else has to hear about it too. `TabClose` already
            // broadcasts, and a session closing is the larger event: without
            // this, another GUI keeps the session in its list -- as an entry
            // whose panes drain one by one and then sits there empty -- until
            // something unrelated makes it re-list.
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionClosed { word_id });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::SessionList`](kmux_protocol::messages::ClientMessage::SessionList).
pub(super) async fn on_session_list(state: &mut SharedClientState, request_id: RequestId) {
    // Merge locally-hosted sessions with every open peer's proxied
    // sessions (local IDs, peer-decorated names), queued in order with
    // every session change (issue #208). Federation off ⇒ the federated
    // list is empty. A closed queue closes the connection on its own.
    let _ = state
        .app
        .send_session_list(&state.ctrl_tx, request_id)
        .await;
}

/// Handle [`ClientMessage::SessionListClosed`](kmux_protocol::messages::ClientMessage::SessionListClosed):
/// this daemon's graveyard (issue #64) and, on a hub, every reachable peer's
/// (issue #228), most recently active first. A peer's are sent only to a
/// client that negotiated `session.closed.peer`: one that does not read
/// `peer` would restore the peer's word from this daemon's graveyard.
pub(super) async fn on_session_list_closed(state: &mut SharedClientState, request_id: RequestId) {
    let peers = if state.negotiated(CAPABILITY_SESSION_CLOSED_PEER) {
        state.app.collect_federated_closed_sessions().await
    } else {
        Vec::new()
    };
    let sessions = merge_closed(state.app.closed_session_entries(), peers);
    state.send(ServerMessage::ClosedSessionListResult {
        request_id,
        sessions,
    });
}

/// This daemon's closed sessions and its peers', as one list ordered most
/// recently active first.
fn merge_closed(
    mut own: Vec<ClosedSessionEntry>,
    peers: Vec<ClosedSessionEntry>,
) -> Vec<ClosedSessionEntry> {
    own.extend(peers);
    own.sort_by_key(|entry| std::cmp::Reverse(entry.last_active_ms));
    own
}

/// Handle [`ClientMessage::SessionRestore`](kmux_protocol::messages::ClientMessage::SessionRestore).
pub(super) async fn on_session_restore(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
) {
    match state.app.restore_session(&word_id).await {
        Ok(entry) => {
            let restored = entry.meta.word_id.clone();
            state.send(ServerMessage::SessionCreated { request_id, entry });
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionCreated { word_id: restored });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

/// Handle [`ClientMessage::SessionRename`](kmux_protocol::messages::ClientMessage::SessionRename).
pub(super) async fn on_session_rename(
    state: &mut SharedClientState,
    request_id: RequestId,
    word_id: WordId,
    new_name: String,
) {
    match state.app.rename_session(&word_id, &new_name).await {
        Ok(()) => {
            state.send(ServerMessage::SessionRenamed {
                word_id: word_id.clone(),
                new_name: new_name.clone(),
            });
            // A name is shared state: every client showing this session in a
            // picker or a tab bar is displaying the old one until it is told.
            // `TabRename` already broadcasts; this did not, so a rename was
            // visible only to whoever performed it.
            state
                .app
                .broadcast_session_event(SessionEventMsg::SessionRenamed { word_id, new_name });
        }
        Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;

    /// A hub's closed list is its own and its peers', most recently active
    /// first, whoever holds each (issue #228).
    #[test]
    fn closed_sessions_merge_most_recently_active_first() {
        use kmux_protocol::messages::{ClosedSessionEntry, SessionMeta};
        let closed = |word: &str, last_active_ms: u64, peer: Option<&str>| ClosedSessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word.to_string(),
                name: word.to_string(),
                cwd: "/tmp".to_string(),
            },
            last_active_ms,
            closed_at_ms: 0,
            pane_count: 1,
            peer: peer.map(Into::into),
        };
        let merged = super::merge_closed(
            vec![closed("hawk", 3, None), closed("wren", 1, None)],
            vec![closed("kite", 2, Some("box"))],
        );
        let words: Vec<&str> = merged.iter().map(|e| e.meta.word_id.as_str()).collect();
        assert_eq!(words, ["hawk", "kite", "wren"]);
    }

    #[tokio::test]
    async fn session_ops_on_an_unknown_target_error_with_the_request_id() {
        let word_id = || MISSING_WORD.to_string();
        assert_all_rejected(vec![
            // Only the federated branch of `SessionCreate` is exercised: the
            // local branch spawns a real PTY, which a unit test must not do.
            Rejected {
                label: "SessionCreate on an unknown peer",
                msg: ClientMessage::SessionCreate {
                    request_id: 1,
                    name: None,
                    cwd: None,
                    program: None,
                    args: vec![],
                    size: TermSize::default(),
                    peer: Some("nosuchpeer".to_string()),
                },
                request_id: Some(1),
                code: ErrorCode::InternalError,
                message: "peer nosuchpeer is not connected".to_string(),
            },
            // A `SessionClosed` reply here would be indistinguishable from a
            // real close, which is what the client treats as confirmation.
            session_not_found(
                "SessionClose",
                Some(2),
                ClientMessage::SessionClose {
                    request_id: 2,
                    word_id: word_id(),
                },
            ),
            session_not_found(
                "SessionRestore",
                Some(11),
                ClientMessage::SessionRestore {
                    request_id: 11,
                    word_id: word_id(),
                    peer: None,
                },
            ),
            session_not_found(
                "SessionRename",
                Some(13),
                ClientMessage::SessionRename {
                    request_id: 13,
                    word_id: word_id(),
                    new_name: "renamed".to_string(),
                },
            ),
        ])
        .await;
    }

    #[tokio::test]
    async fn session_list_on_an_empty_server_returns_an_empty_list() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionList { request_id: 9 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::SessionListResult {
                request_id,
                sessions,
            } => {
                assert_eq!(request_id, 9);
                assert!(sessions.is_empty(), "no sessions exist: {sessions:?}");
            }
            other => panic!("expected SessionListResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_list_closed_on_an_empty_server_returns_an_empty_graveyard() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionListClosed { request_id: 10 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::ClosedSessionListResult {
                request_id,
                sessions,
            } => {
                assert_eq!(request_id, 10);
                assert!(sessions.is_empty(), "the graveyard is empty: {sessions:?}");
            }
            other => panic!("expected ClosedSessionListResult, got {other:?}"),
        }
    }

    /// A hub's closed list holds its peers' closed sessions, tagged with
    /// their peer, for a client that negotiated `session.closed.peer`, and
    /// not for one that did not: that client would restore a peer's word from
    /// the hub's own graveyard (issue #228).
    #[cfg(feature = "federation")]
    #[tokio::test(start_paused = true)]
    async fn a_hubs_closed_list_holds_its_peers_only_for_a_client_that_reads_peer() {
        use kmux_protocol::messages::{CAPABILITY_FRAME_ZSTD, ClosedSessionEntry, SessionMeta};
        let with_peer = protocol_capabilities();
        let without = vec![CAPABILITY_FRAME_ZSTD.to_string()];
        for (offered, listed) in [(with_peer, vec![Some("peer:1")]), (without, vec![])] {
            let (mut state, _out, mut ctrl_rx) =
                fixture_client_state(Arc::new(fixture_app()), TransportKind::Uds);
            authenticate_with_capabilities(&mut state, offered).await;
            let (mut upstream, peer) = state.app.install_channel_peer("fedlocal", "fedremote");
            // Plays the peer: its graveyard holds one session.
            let playing = tokio::spawn(async move {
                while let Some(msg) = upstream.recv().await {
                    if let ClientMessage::SessionListClosed { request_id } = msg {
                        let entry = ClosedSessionEntry {
                            meta: SessionMeta {
                                index: 0,
                                word_id: "kite".to_string(),
                                name: "kite".to_string(),
                                cwd: "/tmp".to_string(),
                            },
                            last_active_ms: 1,
                            closed_at_ms: 1,
                            pane_count: 1,
                            peer: None,
                        };
                        let answer = ServerMessage::ClosedSessionListResult {
                            request_id,
                            sessions: vec![entry],
                        };
                        let _ = peer.send(answer);
                    }
                }
            });
            drain(&mut ctrl_rx);

            let list = ClientMessage::SessionListClosed { request_id: 10 };
            assert!(handle_message(&mut state, list, &NoopAttacher).await);
            playing.abort();

            let peers: Vec<Option<String>> = drain(&mut ctrl_rx)
                .into_iter()
                .find_map(|m| match m {
                    ServerMessage::ClosedSessionListResult { sessions, .. } => Some(sessions),
                    _ => None,
                })
                .expect("the closed list")
                .into_iter()
                .map(|e| e.peer)
                .collect();
            let listed: Vec<Option<String>> =
                listed.into_iter().map(|p| p.map(Into::into)).collect();
            assert_eq!(peers, listed);
        }
    }

    /// A session closing is not the requester's private news. Every other GUI
    /// showing it keeps a stale entry — one whose panes drain away and then sits
    /// there empty — until something unrelated makes it re-list.
    #[tokio::test]
    async fn closing_a_session_tells_every_client_not_only_the_requester() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SessionClose {
                request_id: 30,
                word_id: word.clone(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);

        // The requester still gets its correlated reply.
        match only(drain(&mut ctrl_rx)) {
            ServerMessage::SessionClosed {
                request_id,
                word_id: replied,
                ..
            } => {
                assert_eq!(request_id, 30);
                assert_eq!(replied, word);
            }
            other => panic!("expected SessionClosed, got {other:?}"),
        }

        // And everyone else hears it on the server-wide channel.
        let broadcast = broadcast_event(&mut events, |e| match e {
            SessionEventMsg::SessionClosed { word_id } => Some(word_id),
            _ => None,
        })
        .expect("the close was broadcast to every client");
        assert_eq!(broadcast, word);
    }

    /// A name is shared state: a rename by one GUI has to reach the others, or
    /// their pickers and tab bars keep showing the old one.
    #[tokio::test]
    async fn renaming_a_session_tells_every_client_not_only_the_renamer() {
        let (app, word, mut state, mut ctrl_rx) = app_with_one_session().await;
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SessionRename {
                request_id: 31,
                word_id: word.clone(),
                new_name: "builds".to_string(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);

        match only(drain(&mut ctrl_rx)) {
            ServerMessage::SessionRenamed { word_id, new_name } => {
                assert_eq!(word_id, word);
                assert_eq!(new_name, "builds");
            }
            other => panic!("expected SessionRenamed, got {other:?}"),
        }

        let broadcast = broadcast_event(&mut events, |e| match e {
            SessionEventMsg::SessionRenamed { word_id, new_name } => Some((word_id, new_name)),
            _ => None,
        })
        .expect("the rename was broadcast to every client");
        assert_eq!(broadcast, (word.clone(), "builds".to_string()));

        let _ = app.close_session(&word).await;
    }

    /// The `SessionCreated` reply reaches the creator alone. Without the
    /// broadcast, a session created in one GUI never appeared in another.
    #[tokio::test]
    async fn creating_a_session_tells_every_client_not_only_the_creator() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let app = Arc::clone(&state.app);
        let mut events = app.subscribe_vt_events();

        let keep = handle_message(
            &mut state,
            ClientMessage::SessionCreate {
                request_id: 32,
                name: None,
                cwd: Some("/tmp".to_string()),
                program: Some("/bin/sleep".to_string()),
                args: vec!["30".to_string()],
                size: TermSize::default(),
                peer: None,
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);

        let word = match only(drain(&mut ctrl_rx)) {
            ServerMessage::SessionCreated { request_id, entry } => {
                assert_eq!(request_id, 32);
                entry.meta.word_id
            }
            other => panic!("expected SessionCreated, got {other:?}"),
        };
        let broadcast = broadcast_event(&mut events, |e| match e {
            SessionEventMsg::SessionCreated { word_id } => Some(word_id),
            _ => None,
        })
        .expect("the create was broadcast to every client");
        assert_eq!(broadcast, word);

        let _ = app.close_session(&word).await;
    }
}
