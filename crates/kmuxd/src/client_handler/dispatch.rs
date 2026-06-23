use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use kmux_protocol::messages::{
    ClientMessage, Compression, DirEntry, ErrorCode, ServerMessage, SessionEventMsg, epoch_millis,
};
use kmux_protocol::parse_pane_id;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::{
    AttachParams, AttachResult, ClientIdentity, InputLockOutcome, KickOutcome, PaneCloseOutcome,
};
use crate::auth::validate_token;
use crate::connection::classify_error;

use super::{CLIENT_CHANNEL_CAPACITY, PaneAttacher, PendingAuth, SharedClientState};

/// Build a failed `AuthResult` carrying a human-readable `reason` (issue #146).
fn auth_failure(reason: String) -> ServerMessage {
    ServerMessage::AuthResult {
        success: false,
        reason: Some(reason),
        client_id: None,
        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        connection_id: None,
        compression: None,
        machine_id: None,
        label: None,
        server_machine_id: None,
    }
}

/// Dispatch a single [`ClientMessage`] for a connected client.
///
/// Returns `true` to keep reading, `false` to close the connection.
/// The `attacher` is only called for `ClientMessage::Attach`.
pub async fn handle_message<A: PaneAttacher>(
    state: &mut SharedClientState,
    msg: ClientMessage,
    attacher: &A,
) -> bool {
    if !state.authenticated {
        match msg {
            // Step 1: validate token + protocol, then issue a signing challenge.
            ClientMessage::Auth {
                token,
                protocol_version,
                capabilities,
                connection_id: incoming_conn_id,
                public_key,
                hostname,
                username,
            } => {
                if protocol_version != kmux_protocol::messages::PROTOCOL_VERSION {
                    state.send(auth_failure(format!(
                        "protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    )));
                    warn!(
                        "Protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    );
                    return false;
                }
                if !validate_token(&token, &state.app.auth_token) {
                    state.send(auth_failure("invalid token".to_string()));
                    warn!("authentication failed");
                    return true;
                }
                // Token accepted: challenge the client to prove it holds the
                // private key behind `public_key` (issue #146).
                let nonce = kmux_protocol::identity::random_nonce().to_vec();
                state.pending_auth = Some(PendingAuth {
                    nonce: nonce.clone(),
                    public_key,
                    hostname,
                    username,
                    capabilities,
                    connection_id: incoming_conn_id,
                });
                state.send(ServerMessage::AuthChallenge { nonce });
            }
            // Step 2: verify the signature, then register the connection.
            ClientMessage::AuthProof { signature } => {
                let Some(pending) = state.pending_auth.take() else {
                    state.error(
                        None,
                        ErrorCode::NotAuthenticated,
                        "send Auth before AuthProof",
                    );
                    return true;
                };
                if !kmux_protocol::identity::verify(&pending.public_key, &pending.nonce, &signature)
                {
                    state.send(auth_failure("identity verification failed".to_string()));
                    warn!("identity verification failed");
                    return false;
                }
                let machine_id = kmux_protocol::identity::fingerprint(&pending.public_key);
                let reg = state
                    .app
                    .register_client(
                        state.transport,
                        std::sync::Arc::clone(&state.metrics),
                        pending.connection_id,
                        ClientIdentity {
                            machine_id: machine_id.clone(),
                            hostname: pending.hostname,
                            username: pending.username,
                        },
                    )
                    .await;
                state.client_id = Some(reg.client_id);
                state.connection_id = Some(reg.connection_id);
                state.capabilities = pending.capabilities;
                state.authenticated = true;
                state.pending_swap_from = reg.previous_transport;
                state.machine_id = Some(machine_id.clone());
                state.label = Some(reg.label.clone());
                state.conn_span.record("conn_id", reg.connection_id.0);
                state.conn_span.record("client_id", reg.client_id.0);
                // The daemon decides compression from client locality + config
                // (issue #59). Self-describing frames make this purely a sender
                // policy: flip the shared toggle the writer/attacher tasks read.
                let compress = state.app.compression.enabled_for(state.transport);
                state.comp_out.set_enabled(compress);
                let server_machine_id = state.app.server_machine_id.clone();
                state.send(ServerMessage::AuthResult {
                    success: true,
                    reason: None,
                    client_id: Some(reg.client_id),
                    server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    connection_id: Some(reg.connection_id),
                    compression: compress.then_some(Compression::Zstd),
                    machine_id: Some(machine_id),
                    label: Some(reg.label),
                    server_machine_id: (!server_machine_id.is_empty()).then_some(server_machine_id),
                });
                info!(
                    conn_id = reg.connection_id.0,
                    client_id = reg.client_id.0,
                    label = state.label.as_deref().unwrap_or(""),
                    compress,
                    "client authenticated"
                );
            }
            _ => {
                state.error(None, ErrorCode::NotAuthenticated, "send Auth first");
            }
        }
        return true;
    }

    let client_id = state.client_id.expect("authenticated without client_id");

    match msg {
        ClientMessage::Auth { .. } => {}

        // Already authenticated — a stray proof is ignored.
        ClientMessage::AuthProof { .. } => {}

        ClientMessage::ChannelReady => {
            // The previous transport was captured in `state.pending_swap_from`
            // by the Auth handler at the moment register_client swapped it
            // out. Consuming it here ensures the `ChannelSwitched` reply
            // names the genuine prior transport, even if the registry's
            // recorded transport has since changed (e.g. a third channel
            // arrived). `take` clears the field so a stray duplicate
            // ChannelReady doesn't re-emit a stale switch event.
            if let Some(old) = state.pending_swap_from.take() {
                state.send(ServerMessage::ChannelSwitched {
                    old_transport: old.to_string(),
                });
            }
        }

        ClientMessage::SessionCreate {
            request_id,
            name,
            cwd,
            program,
            args,
            size,
            peer,
        } => match peer {
            // Create on a federated peer (issue #121 launcher): the hub forwards
            // the create upstream and registers the result under a local word,
            // then replies SessionCreated exactly as for a local create.
            Some(peer) => match state
                .app
                .create_remote_session(&peer, name, cwd, program, args, size)
                .await
            {
                Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
                Err(e) => state.error(Some(request_id), ErrorCode::InternalError, e),
            },
            None => match state
                .app
                .create_session(name, cwd, program, args, size, &state.capabilities)
                .await
            {
                Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            },
        },

        ClientMessage::SessionClose {
            request_id,
            word_id,
        } => {
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
                state.app.detach_from_pane(pane_id, client_id).await;
            }
            match state.app.close_session(&word_id).await {
                Ok(exit_code) => state.send(ServerMessage::SessionClosed {
                    request_id,
                    word_id,
                    exit_code,
                }),
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            }
        }

        // `PaneCreate` is the legacy "new pane" intent; under the Session → Tab
        // → Pane model it creates a new TAB (with one pane). The reply still
        // names the new pane so existing clients attach to it as before.
        ClientMessage::PaneCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => match state
            .app
            .create_tab(&word_id, program, args, size, &state.capabilities)
            .await
        {
            Ok((tab, pane)) => {
                let tab_index = tab.tab_index;
                state.send(ServerMessage::PaneCreated {
                    request_id,
                    pane_id: pane.pane_id,
                    session_word_id: word_id.clone(),
                    size,
                });
                state
                    .app
                    .broadcast_session_event(SessionEventMsg::TabCreated { word_id, tab_index });
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::PaneClose {
            request_id,
            pane_id,
        } => {
            if let Some(handle) = state.attached.remove(&pane_id) {
                handle.abort();
            }
            state.app.detach_from_pane(&pane_id, client_id).await;
            match state.app.close_pane(&pane_id).await {
                Ok((exit_code, outcome)) => {
                    state.send(ServerMessage::PaneClosed {
                        request_id,
                        pane_id: pane_id.clone(),
                        exit_code,
                    });
                    let word_id = parse_pane_id(&pane_id)
                        .map(|(w, _)| w.to_string())
                        .unwrap_or_default();
                    match outcome {
                        PaneCloseOutcome::TabUpdated {
                            tab_index,
                            layout,
                            focused_pane,
                        } => state
                            .app
                            .broadcast_layout(&word_id, tab_index, layout, focused_pane),
                        PaneCloseOutcome::TabClosed { tab_index } => state
                            .app
                            .broadcast_session_event(SessionEventMsg::TabClosed {
                                word_id,
                                tab_index,
                            }),
                        PaneCloseOutcome::SessionClosed => state
                            .app
                            .broadcast_session_event(SessionEventMsg::SessionClosed { word_id }),
                        PaneCloseOutcome::Gone => {}
                    }
                }
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::TabCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => match state
            .app
            .create_tab(&word_id, program, args, size, &state.capabilities)
            .await
        {
            Ok((tab, _pane)) => {
                let tab_index = tab.tab_index;
                state.send(ServerMessage::TabCreated {
                    request_id,
                    word_id: word_id.clone(),
                    tab,
                });
                state
                    .app
                    .broadcast_session_event(SessionEventMsg::TabCreated { word_id, tab_index });
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::TabClose {
            request_id,
            word_id,
            tab_index,
        } => match state.app.close_tab(&word_id, tab_index).await {
            Ok(session_closed) => {
                state.send(ServerMessage::TabClosed {
                    request_id,
                    word_id: word_id.clone(),
                    tab_index,
                });
                if session_closed {
                    state
                        .app
                        .broadcast_session_event(SessionEventMsg::SessionClosed { word_id });
                } else {
                    state
                        .app
                        .broadcast_session_event(SessionEventMsg::TabClosed { word_id, tab_index });
                }
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::TabRename {
            request_id,
            word_id,
            tab_index,
            new_name,
        } => match state.app.rename_tab(&word_id, tab_index, &new_name).await {
            Ok(()) => state
                .app
                .broadcast_session_event(SessionEventMsg::TabRenamed {
                    word_id,
                    tab_index,
                    name: new_name,
                }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::PaneSplit {
            request_id,
            word_id,
            tab_index,
            from_pane,
            dir,
            program,
            args,
            size,
        } => match state
            .app
            .split_pane(
                &word_id,
                tab_index,
                from_pane,
                dir,
                program,
                args,
                size,
                &state.capabilities,
            )
            .await
        {
            Ok((new_pane, layout, focused)) => {
                state.send(ServerMessage::PaneSplit {
                    request_id,
                    word_id: word_id.clone(),
                    tab_index,
                    new_pane,
                    layout: layout.clone(),
                });
                state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused);
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::PaneSwap {
            word_id,
            tab_index,
            a,
            b,
        } => {
            if let Ok((layout, focused)) = state.app.swap_panes(&word_id, tab_index, a, b).await {
                state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused);
            }
        }

        ClientMessage::SetLayoutRatios {
            word_id,
            tab_index,
            path,
            ratios,
        } => {
            if let Ok((layout, focused)) = state
                .app
                .set_layout_ratios(&word_id, tab_index, &path, &ratios)
                .await
            {
                state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused);
            }
        }

        ClientMessage::ApplyLayoutScheme {
            word_id,
            tab_index,
            scheme,
        } => {
            if let Ok((layout, focused)) = state
                .app
                .apply_layout_scheme(&word_id, tab_index, scheme)
                .await
            {
                state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused);
            }
        }

        ClientMessage::SetFocus {
            word_id,
            tab_index,
            pane_index,
        } => {
            if let Ok((layout, focused)) = state
                .app
                .set_tab_focus(&word_id, tab_index, pane_index)
                .await
            {
                state
                    .app
                    .broadcast_layout(&word_id, tab_index, layout, focused);
            }
        }

        ClientMessage::SessionList { request_id } => {
            // Merge locally-hosted sessions with every open peer's proxied
            // sessions (local IDs, peer-decorated names). Federation off ⇒ the
            // federated list is empty and this is the original behaviour.
            let mut sessions = state.app.list_sessions().await;
            sessions.extend(state.app.list_federated_sessions());
            state.send(ServerMessage::SessionListResult {
                request_id,
                sessions,
            });
        }

        ClientMessage::ProcessOverview { request_id } => {
            // Merge the locally-hosted panes' process trees with every open
            // peer's (issue #122). Federation off ⇒ the federated half is empty.
            let mut panes = state.app.local_process_overview().await;
            panes.extend(state.app.collect_federated_process_overview().await);
            state.send(ServerMessage::ProcessOverviewResult { request_id, panes });
        }

        ClientMessage::PtyInput { pane_id, data } => {
            if state.app.is_federated_pane(&pane_id) {
                state
                    .app
                    .forward_peer_message(&pane_id, move |remote| ClientMessage::PtyInput {
                        pane_id: remote,
                        data,
                    });
            } else if let Err(e) = state.app.write_input(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::PtyPaste { pane_id, data } => {
            if state.app.is_federated_pane(&pane_id) {
                state
                    .app
                    .forward_peer_message(&pane_id, move |remote| ClientMessage::PtyPaste {
                        pane_id: remote,
                        data,
                    });
            } else if let Err(e) = state.app.write_paste(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::PtyKeyBatch { pane_id, events } => {
            if state.app.is_federated_pane(&pane_id) {
                state.app.forward_peer_message(&pane_id, move |remote| {
                    ClientMessage::PtyKeyBatch {
                        pane_id: remote,
                        events,
                    }
                });
            } else if let Err(e) = state
                .app
                .write_key_batch(&pane_id, client_id, &events)
                .await
            {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Resize { pane_id, size } => {
            // Federated panes reconcile smallest-wins across local viewers inside
            // the peer subsystem (which forwards at most one upstream Resize),
            // rather than forwarding this client's size verbatim.
            if state.app.is_federated_pane(&pane_id) {
                state.app.federated_resize(&pane_id, client_id, size);
            } else if let Err(e) = state.app.resize(&pane_id, client_id, size).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Attach {
            pane_id,
            last_seqno,
            size,
        } => {
            // If already attached, detach first (routes to the peer subsystem
            // for federated panes, the local relay otherwise).
            if let Some(old) = state.attached.remove(&pane_id) {
                old.abort();
                state.app.detach_pane_any(&pane_id, client_id).await;
            }

            let (client_tx, client_rx) = mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

            if state.app.is_federated_pane(&pane_id) {
                // Federated pane: register as a viewer and forward the `Attach`
                // upstream. The remote's snapshot and diffs arrive asynchronously
                // through the peer feed loop and are pumped to this client via
                // `client_rx`, so the synchronous replay is empty — `Delta(vec![])`
                // emits no initial frames (see `build_attach_replay`).
                state.app.federated_attach(
                    &pane_id,
                    client_id,
                    client_tx,
                    state.ctrl_tx.clone(),
                    last_seqno,
                    size,
                );
                match attacher
                    .start_pane_stream(pane_id.clone(), AttachResult::Delta(vec![]), client_rx)
                    .await
                {
                    Ok(handle) => {
                        state.attached.insert(pane_id, handle);
                    }
                    Err(e) => state.error(None, ErrorCode::InternalError, e),
                }
            } else {
                match state
                    .app
                    .attach(AttachParams {
                        pane_id: pane_id.clone(),
                        client_id,
                        last_seqno,
                        size,
                        data_tx: client_tx,
                        ctrl_tx: state.ctrl_tx.clone(),
                        capabilities: state.capabilities.clone(),
                    })
                    .await
                {
                    Ok(result) => {
                        match attacher
                            .start_pane_stream(pane_id.clone(), result, client_rx)
                            .await
                        {
                            Ok(handle) => {
                                state.attached.insert(pane_id, handle);
                            }
                            Err(e) => {
                                state.error(None, ErrorCode::InternalError, e);
                            }
                        }
                    }
                    Err(e) => state.error(None, classify_error(&e), e.to_string()),
                }
            }
        }

        ClientMessage::Detach { pane_id } => {
            if let Some(handle) = state.attached.remove(&pane_id) {
                handle.abort();
                state.app.detach_pane_any(&pane_id, client_id).await;
                debug!("detached from pane '{pane_id}'");
            }
        }

        ClientMessage::Signal { pane_id, signal } => {
            if state.app.is_federated_pane(&pane_id) {
                state
                    .app
                    .forward_peer_message(&pane_id, move |remote| ClientMessage::Signal {
                        pane_id: remote,
                        signal,
                    });
            } else if let Err(e) = state.app.send_signal(&pane_id, signal).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::RequestInputLock { pane_id } => {
            match state.app.request_input_lock(&pane_id, client_id).await {
                Ok(InputLockOutcome::Granted) => {
                    state.send(ServerMessage::InputLockGranted { pane_id });
                }
                Ok(InputLockOutcome::Denied(holder)) => {
                    state.send(ServerMessage::InputLockDenied { pane_id, holder });
                }
                Err(e) => state.error(None, classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::ReleaseInputLock { pane_id } => {
            match state.app.release_input_lock(&pane_id, client_id).await {
                Ok(true) => state.send(ServerMessage::InputLockReleased { pane_id }),
                Ok(false) => {}
                Err(e) => state.error(None, classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::SessionRename {
            request_id,
            word_id,
            new_name,
        } => match state.app.rename_session(&word_id, &new_name).await {
            Ok(()) => state.send(ServerMessage::SessionRenamed { word_id, new_name }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::SetSnapshotMode { enabled } => {
            state.app.set_snapshot_mode(client_id, enabled).await;
            debug!("client {client_id:?} snapshot mode = {enabled}");
        }

        ClientMessage::SetPaused { paused, auto } => {
            state.app.set_paused(client_id, paused, auto).await;
            debug!("client {client_id:?} paused = {paused} (auto = {auto})");
        }

        ClientMessage::SetPaneNoAutoPause { pane_id, exempt } => {
            state
                .app
                .set_pane_no_auto_pause(client_id, &pane_id, exempt)
                .await;
            debug!("client {client_id:?} pane {pane_id} no_auto_pause = {exempt}");
        }

        ClientMessage::FetchHistory {
            request_id,
            pane_id,
            start_index,
            count,
        } => {
            // For a federated pane, forward the request upstream; the remote's
            // `HistoryLines` reply is pane-scoped, so the feed loop translates it
            // back to this viewer (matched by `request_id`).
            if state.app.is_federated_pane(&pane_id) {
                state.app.forward_peer_message(&pane_id, move |remote| {
                    ClientMessage::FetchHistory {
                        request_id,
                        pane_id: remote,
                        start_index,
                        count,
                    }
                });
            } else {
                match state.app.fetch_history(&pane_id, start_index, count).await {
                    Ok((first_index, lines, history_total)) => {
                        state.send(ServerMessage::HistoryLines {
                            request_id,
                            pane_id,
                            first_index,
                            lines,
                            history_total,
                            sent_at_ms: kmux_protocol::messages::epoch_millis(),
                        });
                    }
                    Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
                }
            }
        }

        ClientMessage::ListDirectory { request_id, path } => {
            state.send(list_directory(request_id, &path));
        }

        ClientMessage::OpenPeer { request_id, target } => {
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

        ClientMessage::ClosePeer { request_id, peer } => {
            state.app.close_peer(&peer);
            state.send(ServerMessage::PeerClosed { request_id, peer });
        }

        ClientMessage::ClientList {
            request_id,
            word_id,
        } => {
            // Federated session ⇒ forward to the owning peer; otherwise build the
            // list from this daemon's own connections (issue #146).
            if state.app.is_federated_session(&word_id) {
                match state.app.list_federated_session_clients(&word_id).await {
                    Ok(clients) => state.send(ServerMessage::ClientListResult {
                        request_id,
                        word_id,
                        clients,
                    }),
                    Err(reason) => {
                        state.error(Some(request_id), ErrorCode::SessionNotFound, reason)
                    }
                }
            } else {
                match state.app.list_session_clients(&word_id, client_id).await {
                    Some(clients) => state.send(ServerMessage::ClientListResult {
                        request_id,
                        word_id,
                        clients,
                    }),
                    None => state.error(
                        Some(request_id),
                        ErrorCode::SessionNotFound,
                        "session not found",
                    ),
                }
            }
        }

        ClientMessage::KickClient {
            request_id,
            word_id,
            client_id: target,
        } => {
            if state.app.is_federated_session(&word_id) {
                match state.app.kick_federated_client(&word_id, target).await {
                    Ok(()) => state.send(ServerMessage::ClientKicked {
                        request_id,
                        word_id,
                        client_id: target,
                    }),
                    Err(reason) => state.error(Some(request_id), ErrorCode::ClientNotFound, reason),
                }
            } else {
                let by_label = state.label.clone().unwrap_or_default();
                match state
                    .app
                    .kick_client_from_session(&word_id, target, &by_label)
                    .await
                {
                    KickOutcome::Kicked => state.send(ServerMessage::ClientKicked {
                        request_id,
                        word_id,
                        client_id: target,
                    }),
                    KickOutcome::SessionNotFound => state.error(
                        Some(request_id),
                        ErrorCode::SessionNotFound,
                        "session not found",
                    ),
                    KickOutcome::ClientNotFound => state.error(
                        Some(request_id),
                        ErrorCode::ClientNotFound,
                        "client not attached to session",
                    ),
                }
            }
        }

        ClientMessage::Ping { seq } => {
            state.send(ServerMessage::Pong { seq });
        }

        ClientMessage::Pong { seq } => {
            let sent = *state.metrics.last_ping_sent.lock().unwrap();
            if let Some((sent_seq, sent_at)) = sent
                && sent_seq == seq
            {
                let rtt_ms = sent_at.elapsed().as_millis() as u64;
                state.metrics.last_rtt_ms.store(rtt_ms, Ordering::Relaxed);
                state
                    .metrics
                    .last_pong_ms
                    .store(epoch_millis(), Ordering::Relaxed);
            }
        }
    }

    true
}

/// Maximum number of directory entries returned in a single `DirectoryListing`,
/// to bound the reply size for very large directories.
const MAX_DIR_ENTRIES: usize = 2000;

/// Build the `DirectoryListing` reply for a `ListDirectory` request.
///
/// Resolves `requested` (empty ⇒ `$HOME`, else the daemon's `.`), canonicalizes
/// it, and returns its **subdirectories only** (the browser is choosing a
/// directory), sorted case-insensitively and capped at [`MAX_DIR_ENTRIES`]. On
/// any IO error it returns `error: Some(..)` with empty `entries` and echoes the
/// requested path so the client keeps showing where it tried to go. This reads
/// the daemon's own filesystem (the user owns it), so no sandboxing is applied
/// beyond normal filesystem permissions.
fn list_directory(request_id: u64, requested: &str) -> ServerMessage {
    let target = if requested.is_empty() {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(requested)
    };

    let canonical = match std::fs::canonicalize(&target) {
        Ok(p) => p,
        Err(e) => return directory_error(request_id, requested, &e),
    };

    let read = match std::fs::read_dir(&canonical) {
        Ok(rd) => rd,
        Err(e) => return directory_error(request_id, requested, &e),
    };

    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in read.flatten() {
        // Skip entries whose metadata can't be read (e.g. dangling symlink) and
        // anything that is not a directory — `file_type()` does not traverse
        // symlinks, so a symlink loop can't recurse here.
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {}
            Ok(ft) if ft.is_symlink() => {
                // Resolve one level so symlinked directories still appear, but
                // bail out gracefully if the link is broken or loops.
                match std::fs::metadata(entry.path()) {
                    Ok(md) if md.is_dir() => {}
                    _ => continue,
                }
            }
            _ => continue,
        }
        if let Some(name) = entry.file_name().to_str() {
            entries.push(DirEntry {
                name: name.to_string(),
                is_dir: true,
            });
        }
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    entries.truncate(MAX_DIR_ENTRIES);

    let parent = canonical
        .parent()
        .and_then(Path::to_str)
        .map(str::to_string);

    ServerMessage::DirectoryListing {
        request_id,
        path: canonical.to_string_lossy().into_owned(),
        parent,
        entries,
        error: None,
    }
}

/// Build a failed `DirectoryListing` echoing the requested path.
fn directory_error(request_id: u64, requested: &str, err: &std::io::Error) -> ServerMessage {
    ServerMessage::DirectoryListing {
        request_id,
        path: requested.to_string(),
        parent: None,
        entries: vec![],
        error: Some(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use kmux_protocol::Compressor;
    use kmux_protocol::TransportKind;
    use kmux_protocol::messages::{
        ClientCapabilities, ClientMessage, Compression, PROTOCOL_VERSION, ServerMessage,
    };
    use tokio::sync::mpsc;
    use tokio::task::AbortHandle;

    use super::handle_message;
    use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
    use crate::client_handler::{OutboundCompression, PaneAttacher, SharedClientState};
    use crate::config::{CompressionConfig, CompressionMode};

    /// Auth doesn't attach panes, so a never-called stub attacher suffices.
    struct NoopAttacher;
    impl PaneAttacher for NoopAttacher {
        fn start_pane_stream(
            &self,
            _pane_id: String,
            _result: AttachResult,
            _client_rx: mpsc::Receiver<ServerMessage>,
        ) -> impl std::future::Future<Output = Result<AbortHandle, String>> + Send {
            // Never invoked during auth; `ready` avoids an empty async block.
            std::future::ready(Err("noop".to_string()))
        }
    }

    fn state_for(
        app: Arc<ServerApp>,
        transport: TransportKind,
    ) -> (
        SharedClientState,
        Arc<OutboundCompression>,
        mpsc::UnboundedReceiver<ServerMessage>,
    ) {
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel();
        let comp_out = Arc::new(OutboundCompression::new(
            app.compression.level,
            app.compression.min_size,
        ));
        let state = SharedClientState::new(
            app,
            ctrl_tx,
            tracing::Span::none(),
            transport,
            Arc::new(ConnectionMetrics::new()),
            Arc::clone(&comp_out),
        );
        (state, comp_out, ctrl_rx)
    }

    async fn authenticate(state: &mut SharedClientState) {
        let identity = kmux_protocol::identity::Identity::generate();
        // Step 1: Auth → the daemon stashes a challenge in `state.pending_auth`.
        let ok = handle_message(
            state,
            ClientMessage::Auth {
                token: "tok".to_string(),
                protocol_version: PROTOCOL_VERSION,
                capabilities: ClientCapabilities::default(),
                connection_id: None,
                public_key: identity.public_key_bytes().to_vec(),
                hostname: "host".to_string(),
                username: "user".to_string(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(ok, "auth must keep the connection open");
        let nonce = state
            .pending_auth
            .as_ref()
            .expect("challenge issued after valid token")
            .nonce
            .clone();
        // Step 2: AuthProof with a valid signature over the nonce.
        let ok = handle_message(
            state,
            ClientMessage::AuthProof {
                signature: identity.sign(&nonce),
            },
            &NoopAttacher,
        )
        .await;
        assert!(ok, "auth proof must keep the connection open");
        assert!(
            state.authenticated,
            "auth must succeed with a matching token + valid identity proof"
        );
    }

    /// With `mode = always`, a networked transport negotiates zstd: the auth
    /// handler flips the shared toggle and advertises it in `AuthResult`.
    #[tokio::test]
    async fn auth_enables_compression_when_policy_says_so() {
        let app = Arc::new(
            ServerApp::new("tok".to_string()).with_compression(CompressionConfig {
                mode: CompressionMode::Always,
                ..CompressionConfig::default()
            }),
        );
        let (mut state, comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::TcpTls);
        authenticate(&mut state).await;

        assert!(
            matches!(comp_out.compressor(), Compressor::Zstd { .. }),
            "writer-side compression must be enabled"
        );
        // The challenge precedes the result on the control channel.
        assert!(matches!(
            ctrl_rx.try_recv().expect("AuthChallenge queued"),
            ServerMessage::AuthChallenge { .. }
        ));
        let auth = ctrl_rx.try_recv().expect("AuthResult queued");
        assert!(matches!(
            auth,
            ServerMessage::AuthResult {
                success: true,
                compression: Some(Compression::Zstd),
                ..
            }
        ));
    }

    /// Under the default `auto` mode a local UDS client is left uncompressed.
    #[tokio::test]
    async fn auth_leaves_uds_uncompressed_under_auto() {
        let app = Arc::new(ServerApp::new("tok".to_string())); // default compression = auto
        let (mut state, comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        authenticate(&mut state).await;

        assert!(
            matches!(comp_out.compressor(), Compressor::Off),
            "local UDS clients must stay uncompressed under auto"
        );
        // The challenge precedes the result on the control channel.
        assert!(matches!(
            ctrl_rx.try_recv().expect("AuthChallenge queued"),
            ServerMessage::AuthChallenge { .. }
        ));
        let auth = ctrl_rx.try_recv().expect("AuthResult queued");
        assert!(matches!(
            auth,
            ServerMessage::AuthResult {
                success: true,
                compression: None,
                ..
            }
        ));
    }

    /// A valid token with an invalid identity signature is rejected and the
    /// connection is closed (issue #146): proof-of-possession is mandatory.
    #[tokio::test]
    async fn auth_rejects_invalid_signature() {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        let identity = kmux_protocol::identity::Identity::generate();

        let ok = handle_message(
            &mut state,
            ClientMessage::Auth {
                token: "tok".to_string(),
                protocol_version: PROTOCOL_VERSION,
                capabilities: ClientCapabilities::default(),
                connection_id: None,
                public_key: identity.public_key_bytes().to_vec(),
                hostname: "h".to_string(),
                username: "u".to_string(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(
            ok,
            "a valid token keeps the connection open for the proof step"
        );
        assert!(matches!(
            ctrl_rx.try_recv(),
            Ok(ServerMessage::AuthChallenge { .. })
        ));

        // A bogus signature must be rejected and the connection closed.
        let ok = handle_message(
            &mut state,
            ClientMessage::AuthProof {
                signature: vec![0u8; 64],
            },
            &NoopAttacher,
        )
        .await;
        assert!(!ok, "an invalid proof must close the connection");
        assert!(!state.authenticated);
        assert!(matches!(
            ctrl_rx.try_recv(),
            Ok(ServerMessage::AuthResult { success: false, .. })
        ));
    }

    #[test]
    fn list_directory_returns_sorted_dirs_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("zebra")).unwrap();
        std::fs::create_dir(tmp.path().join("Alpha")).unwrap();
        std::fs::write(tmp.path().join("a_file.txt"), b"hi").unwrap();

        let msg = list_directory(1, tmp.path().to_str().unwrap());
        match msg {
            ServerMessage::DirectoryListing {
                request_id,
                entries,
                error,
                parent,
                ..
            } => {
                assert_eq!(request_id, 1);
                assert!(error.is_none());
                assert!(parent.is_some(), "a tempdir has a parent");
                let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
                // Files are excluded; dirs are sorted case-insensitively.
                assert_eq!(names, vec!["Alpha", "zebra"]);
                assert!(entries.iter().all(|e| e.is_dir));
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn list_directory_reports_error_for_missing_path() {
        let msg = list_directory(2, "/this/path/does/not/exist/kmux");
        match msg {
            ServerMessage::DirectoryListing {
                path,
                entries,
                error,
                ..
            } => {
                assert_eq!(path, "/this/path/does/not/exist/kmux");
                assert!(entries.is_empty());
                assert!(error.is_some());
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

    #[test]
    fn list_directory_empty_path_resolves_a_default() {
        // An empty path resolves to $HOME (or "."); either way it must not error
        // in a normal environment and must echo a canonical, absolute path.
        let msg = list_directory(3, "");
        match msg {
            ServerMessage::DirectoryListing { path, error, .. } => {
                assert!(error.is_none(), "default dir should list: {error:?}");
                assert!(
                    Path::new(&path).is_absolute(),
                    "canonicalized path should be absolute: {path}"
                );
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }
}
