use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use kmux_protocol::messages::{
    CAPABILITY_FRAME_ZSTD, ClientMessage, Compression, DirEntry, ErrorCode, ServerMessage,
    SessionEventMsg, epoch_millis, negotiate_capabilities,
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
        negotiated_protocol: None,
        negotiated_capabilities: Vec::new(),
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
                protocol_range,
                protocol_capabilities,
                capabilities,
                connection_id: incoming_conn_id,
                public_key,
                hostname,
                username,
                client_kind,
                client_git_sha,
                client_git_dirty,
                client_build_profile,
            } => {
                let Some(negotiated_protocol) =
                    kmux_protocol::compat::negotiate_protocol(protocol_range)
                else {
                    state.send(auth_failure(format!(
                        "protocol version mismatch: client={protocol_range}, server={}",
                        kmux_protocol::messages::PROTOCOL_RANGE
                    )));
                    warn!(
                        "Protocol version mismatch: client={protocol_range}, server={}",
                        kmux_protocol::messages::PROTOCOL_RANGE
                    );
                    return false;
                };
                let negotiated_capabilities = negotiate_capabilities(&protocol_capabilities);
                if !validate_token(&token, &state.app.auth_token) {
                    state.send(auth_failure("invalid token".to_string()));
                    warn!("authentication failed");
                    return false;
                }
                // Token accepted: challenge the client to prove it holds the
                // private key behind `public_key` (issue #146).
                let nonce = kmux_sys::identity::random_nonce().to_vec();
                state.pending_auth = Some(PendingAuth {
                    nonce: nonce.clone(),
                    public_key,
                    hostname,
                    username,
                    capabilities,
                    negotiated_protocol,
                    negotiated_capabilities,
                    connection_id: incoming_conn_id,
                    client_kind,
                    client_git_sha,
                    client_git_dirty,
                    client_build_profile,
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
                if !kmux_sys::identity::verify(&pending.public_key, &pending.nonce, &signature) {
                    state.send(auth_failure("identity verification failed".to_string()));
                    warn!("identity verification failed");
                    return false;
                }
                let machine_id = kmux_sys::identity::fingerprint(&pending.public_key);
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
                            client_kind: pending.client_kind,
                            client_git_sha: pending.client_git_sha,
                            client_git_dirty: pending.client_git_dirty,
                            client_build_profile: pending.client_build_profile,
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
                let compress = pending
                    .negotiated_capabilities
                    .iter()
                    .any(|capability| capability == CAPABILITY_FRAME_ZSTD)
                    && state.app.compression.enabled_for(state.transport);
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
                    negotiated_protocol: Some(pending.negotiated_protocol),
                    negotiated_capabilities: pending.negotiated_capabilities,
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

        ClientMessage::TabReorder {
            word_id,
            tab_index,
            new_position,
        } => match state
            .app
            .reorder_tab(&word_id, tab_index, new_position)
            .await
        {
            Ok(tab_indices) => state
                .app
                .broadcast_session_event(SessionEventMsg::TabsReordered {
                    word_id,
                    tab_indices,
                }),
            Err(e) => state.error(None, classify_error(&e), e.to_string()),
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

        // Closed-session restore (issue #64). The graveyard is local-only, so
        // these are not federated.
        ClientMessage::SessionListClosed { request_id } => {
            state.send(ServerMessage::ClosedSessionListResult {
                request_id,
                sessions: state.app.closed_session_entries(),
            });
        }

        ClientMessage::SessionRestore {
            request_id,
            word_id,
        } => match state.app.restore_session(&word_id).await {
            Ok(entry) => {
                let restored = entry.meta.word_id.clone();
                state.send(ServerMessage::SessionCreated { request_id, entry });
                state
                    .app
                    .broadcast_session_event(SessionEventMsg::SessionCreated { word_id: restored });
            }
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

        ClientMessage::ProcessOverview { request_id } => {
            // Merge the locally-hosted panes' process trees with every open
            // peer's (issue #122). Federation off ⇒ the federated half is empty.
            let mut panes = state.app.local_process_overview().await;
            panes.extend(state.app.collect_federated_process_overview().await);
            state.send(ServerMessage::ProcessOverviewResult { request_id, panes });
        }

        // Stream this daemon's own log file back to the client (issue #187), so
        // `kmux daemon logs --server <host>` can read a remote daemon's log. The
        // local form reads the file off disk; only the daemon log is reachable
        // across machines, so there is no federated/peer-forwarded variant.
        ClientMessage::FetchLogs {
            request_id,
            lines,
            follow,
        } => {
            handle_fetch_logs(state, request_id, lines, follow).await;
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
                            sent_at_ms: epoch_millis(),
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
                        state.error(Some(request_id), ErrorCode::SessionNotFound, reason);
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

        ClientMessage::Notify {
            request_id,
            pane_id,
            kind,
            title,
            body,
        } => match state
            .app
            .notify_pane_attention(&pane_id, kind, title, body)
            .await
        {
            Ok(()) => state.send(ServerMessage::NotifyAccepted { request_id }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
        },

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

/// Chunk size for streaming a log file to a client (issue #187): large enough
/// that framing overhead is negligible, small enough to bound per-message memory.
const LOG_CHUNK_BYTES: usize = 64 * 1024;

/// Answer a [`ClientMessage::FetchLogs`] (issue #187): stream this daemon's own
/// log file to the client over the control channel.
///
/// Sends the existing content (trimmed to the last `lines` lines when set) as
/// `LogChunk`s, then either a terminating `LogEnd` or — under `follow` — spawns a
/// detached task that tails the file and keeps pushing `LogChunk`s until the
/// connection's writer is gone (its `ctrl_tx` is closed). The follow task checks
/// `ctrl_tx.is_closed()` each tick so a disconnect during an idle log never
/// leaks the task.
async fn handle_fetch_logs(
    state: &SharedClientState,
    request_id: u64,
    lines: Option<u32>,
    follow: bool,
) {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let path = match kmux_sys::dirs::daemon_log_path() {
        Ok(p) => p,
        Err(e) => {
            state.error(
                Some(request_id),
                ErrorCode::InternalError,
                format!("daemon log path unavailable: {e}"),
            );
            return;
        }
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            state.error(
                Some(request_id),
                ErrorCode::InternalError,
                format!("daemon log not readable at {}: {e}", path.display()),
            );
            return;
        }
    };

    let mut buf = Vec::new();
    if let Err(e) = file.read_to_end(&mut buf).await {
        state.error(
            Some(request_id),
            ErrorCode::InternalError,
            format!("reading daemon log failed: {e}"),
        );
        return;
    }

    let start = match lines {
        Some(n) => kmux_sys::log_tail::last_n_lines_offset(&buf, n as usize),
        None => 0,
    };
    for chunk in buf[start..].chunks(LOG_CHUNK_BYTES) {
        state.send(ServerMessage::LogChunk {
            request_id,
            data: chunk.to_vec(),
        });
    }

    if !follow {
        state.send(ServerMessage::LogEnd { request_id });
        return;
    }

    // Follow: tail appended bytes from the current end of file. `read_to_end`
    // already left the cursor at EOF, but seek explicitly to be sure.
    let ctrl_tx = state.ctrl_tx.clone();
    tokio::spawn(async move {
        if file.seek(std::io::SeekFrom::End(0)).await.is_err() {
            return;
        }
        let mut read_buf = vec![0u8; LOG_CHUNK_BYTES];
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            if ctrl_tx.is_closed() {
                return;
            }
            match file.read(&mut read_buf).await {
                Ok(0) => continue,
                Ok(n) => {
                    if ctrl_tx
                        .send(ServerMessage::LogChunk {
                            request_id,
                            data: read_buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    });
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
        std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
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
        ClientCapabilities, ClientMessage, Compression, PROTOCOL_RANGE, ProtocolRange,
        ProtocolVersion, ServerMessage, protocol_capabilities,
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
        ) -> impl Future<Output = Result<AbortHandle, String>> + Send {
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

    async fn authenticate_with_capabilities(
        state: &mut SharedClientState,
        protocol_capabilities: Vec<String>,
    ) {
        let identity = kmux_sys::identity::Identity::generate();
        // Step 1: Auth → the daemon stashes a challenge in `state.pending_auth`.
        let ok = handle_message(
            state,
            ClientMessage::Auth {
                token: "tok".to_string(),
                protocol_range: PROTOCOL_RANGE,
                protocol_capabilities,
                capabilities: ClientCapabilities::default(),
                connection_id: None,
                public_key: identity.public_key_bytes().to_vec(),
                hostname: "host".to_string(),
                username: "user".to_string(),
                client_kind: kmux_protocol::messages::FrontendKind::Cli,
                client_git_sha: String::new(),
                client_git_dirty: false,
                client_build_profile: String::new(),
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

    async fn authenticate(state: &mut SharedClientState) {
        authenticate_with_capabilities(state, protocol_capabilities()).await;
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

    #[tokio::test]
    async fn auth_does_not_use_unadvertised_compression_capability() {
        let app = Arc::new(
            ServerApp::new("tok".to_string()).with_compression(CompressionConfig {
                mode: CompressionMode::Always,
                ..CompressionConfig::default()
            }),
        );
        let (mut state, comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::TcpTls);
        authenticate_with_capabilities(&mut state, Vec::new()).await;

        assert!(matches!(comp_out.compressor(), Compressor::Off));
        assert!(matches!(
            ctrl_rx.try_recv(),
            Ok(ServerMessage::AuthChallenge { .. })
        ));
        assert!(matches!(
            ctrl_rx.try_recv(),
            Ok(ServerMessage::AuthResult {
                success: true,
                compression: None,
                negotiated_capabilities,
                ..
            }) if negotiated_capabilities.is_empty()
        ));
    }

    #[tokio::test]
    async fn auth_rejects_disjoint_protocol_range_before_token_validation() {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(app, TransportKind::Uds);
        let ok = handle_message(
            &mut state,
            ClientMessage::Auth {
                token: "tok".to_string(),
                protocol_range: ProtocolRange::exact(ProtocolVersion::new(2, 0, 0)),
                protocol_capabilities: Vec::new(),
                capabilities: ClientCapabilities::default(),
                connection_id: None,
                public_key: Vec::new(),
                hostname: "host".to_string(),
                username: "user".to_string(),
                client_kind: kmux_protocol::messages::FrontendKind::Cli,
                client_git_sha: String::new(),
                client_git_dirty: false,
                client_build_profile: String::new(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(!ok);
        assert!(matches!(
            ctrl_rx.try_recv(),
            Ok(ServerMessage::AuthResult {
                success: false,
                reason: Some(reason),
                ..
            }) if reason.starts_with("protocol version mismatch:")
        ));
    }

    /// A valid token with an invalid identity signature is rejected and the
    /// connection is closed (issue #146): proof-of-possession is mandatory.
    #[tokio::test]
    async fn auth_rejects_invalid_signature() {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        let identity = kmux_sys::identity::Identity::generate();

        let ok = handle_message(
            &mut state,
            ClientMessage::Auth {
                token: "tok".to_string(),
                protocol_range: PROTOCOL_RANGE,
                protocol_capabilities: protocol_capabilities(),
                capabilities: ClientCapabilities::default(),
                connection_id: None,
                public_key: identity.public_key_bytes().to_vec(),
                hostname: "h".to_string(),
                username: "u".to_string(),
                client_kind: kmux_protocol::messages::FrontendKind::Cli,
                client_git_sha: String::new(),
                client_git_dirty: false,
                client_build_profile: String::new(),
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

    // ─── Per-arm characterization ────────────────────────────────────────────
    // One test per `ClientMessage` variant, against an authenticated client and
    // an empty server (no sessions, no panes). These pin what each arm of
    // `handle_message` does before it is split into per-domain handlers; see
    // docs/testing.md R2 and R4. Every expectation below was read off the
    // running code rather than invented; where the observed behaviour looks
    // wrong it is recorded faithfully and flagged with `// SUSPECT:` instead of
    // being "corrected" here.

    use kmux_protocol::messages::{
        AttentionKind, ClientId, KeyAction, KeyCode, KeyEvent, KeyMods, LayoutScheme, PeerTarget,
        SplitDir, TermSize,
    };

    /// A word id no session uses, so every session-scoped arm takes its
    /// not-found path.
    const MISSING_WORD: &str = "nosuch";
    /// A well-formed pane id (`word/index`) that parses but resolves to nothing.
    const MISSING_PANE: &str = "nosuch/0";

    /// An authenticated client on an empty server, with the handshake replies
    /// already drained so an assertion sees only the arm under test.
    async fn authenticated_client() -> (SharedClientState, mpsc::UnboundedReceiver<ServerMessage>) {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(app, TransportKind::Uds);
        authenticate(&mut state).await;
        while ctrl_rx.try_recv().is_ok() {}
        (state, ctrl_rx)
    }

    /// Everything queued on the control channel, in order.
    fn drain(ctrl_rx: &mut mpsc::UnboundedReceiver<ServerMessage>) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = ctrl_rx.try_recv() {
            out.push(msg);
        }
        out
    }

    /// Dispatch exactly one message to a freshly authenticated client on an
    /// empty server; returns the keep-reading flag and everything it emitted.
    async fn dispatch_one(msg: ClientMessage) -> (bool, Vec<ServerMessage>) {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(&mut state, msg, &NoopAttacher).await;
        let out = drain(&mut ctrl_rx);
        (keep, out)
    }

    /// The single message an arm emitted; panics when it emitted zero or many.
    fn only(msgs: Vec<ServerMessage>) -> ServerMessage {
        assert_eq!(msgs.len(), 1, "expected exactly one reply, got {msgs:?}");
        msgs.into_iter().next().expect("length asserted above")
    }

    /// The parts of the single `Error` an arm emitted.
    fn only_error(msgs: Vec<ServerMessage>) -> (Option<u64>, ErrorCode, String) {
        match only(msgs) {
            ServerMessage::Error {
                request_id,
                code,
                message,
            } => (request_id, code, message),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A single printable keystroke, so `PtyKeyBatch` gets past its
    /// empty-batch short circuit and reaches the pane lookup.
    fn one_key() -> KeyEvent {
        KeyEvent {
            code: KeyCode::A,
            mods: KeyMods::empty(),
            action: KeyAction::Press,
            text: "a".to_string(),
            unshifted_codepoint: u32::from('a'),
        }
    }

    #[tokio::test]
    async fn an_unauthenticated_client_is_told_to_send_auth_first() {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(app, TransportKind::Uds);
        let keep = handle_message(&mut state, ClientMessage::Ping { seq: 1 }, &NoopAttacher).await;
        assert!(keep, "the pre-auth gate keeps the connection open");
        let (request_id, code, message) = only_error(drain(&mut ctrl_rx));
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::NotAuthenticated);
        assert_eq!(message, "send Auth first");
    }

    #[tokio::test]
    async fn a_second_auth_after_authentication_is_ignored_silently() {
        let (keep, msgs) = dispatch_one(ClientMessage::Auth {
            token: "tok".to_string(),
            protocol_range: PROTOCOL_RANGE,
            protocol_capabilities: protocol_capabilities(),
            capabilities: ClientCapabilities::default(),
            connection_id: None,
            public_key: Vec::new(),
            hostname: "host".to_string(),
            username: "user".to_string(),
            client_kind: kmux_protocol::messages::FrontendKind::Cli,
            client_git_sha: String::new(),
            client_git_dirty: false,
            client_build_profile: String::new(),
        })
        .await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "a duplicate Auth answers nothing: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn a_stray_auth_proof_after_authentication_is_ignored_silently() {
        let (keep, msgs) = dispatch_one(ClientMessage::AuthProof {
            signature: vec![0u8; 64],
        })
        .await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "a stray AuthProof answers nothing: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn channel_ready_without_a_pending_swap_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::ChannelReady).await;
        assert!(keep);
        assert!(msgs.is_empty(), "no swap was pending: {msgs:?}");
    }

    #[tokio::test]
    async fn channel_ready_reports_the_pending_swap_and_consumes_it() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        state.pending_swap_from = Some(TransportKind::TcpTls);

        let keep = handle_message(&mut state, ClientMessage::ChannelReady, &NoopAttacher).await;
        assert!(keep);
        match only(drain(&mut ctrl_rx)) {
            ServerMessage::ChannelSwitched { old_transport } => {
                assert_eq!(old_transport, "TCP+TLS");
            }
            other => panic!("expected ChannelSwitched, got {other:?}"),
        }

        // A duplicate `ChannelReady` must not re-emit a stale switch event.
        let keep = handle_message(&mut state, ClientMessage::ChannelReady, &NoopAttacher).await;
        assert!(keep);
        assert!(drain(&mut ctrl_rx).is_empty(), "the swap was consumed");
    }

    #[tokio::test]
    async fn session_create_on_an_unknown_peer_errors_naming_the_peer() {
        // Only the federated branch is exercised: the local branch spawns a real
        // PTY, which a unit test must not do.
        let (keep, msgs) = dispatch_one(ClientMessage::SessionCreate {
            request_id: 1,
            name: None,
            cwd: None,
            program: None,
            args: vec![],
            size: TermSize::default(),
            peer: Some("nosuchpeer".to_string()),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(1));
        assert_eq!(code, ErrorCode::InternalError);
        assert_eq!(message, "peer nosuchpeer is not connected");
    }

    #[tokio::test]
    async fn session_close_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionClose {
            request_id: 2,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        // A `SessionClosed` reply here would be indistinguishable from a real
        // close, which is what the client treats as confirmation.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(2));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn pane_create_for_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneCreate {
            request_id: 3,
            word_id: MISSING_WORD.to_string(),
            program: None,
            args: vec![],
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(3));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn pane_close_of_an_unknown_pane_errors_naming_the_pane_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneClose {
            request_id: 4,
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(4));
        // Every pane-scoped arm below reports `PaneNotFound`; the three ways a
        // lookup can miss (unparseable id, unknown session, unknown index) are
        // deliberately indistinguishable to the client.
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn tab_create_for_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabCreate {
            request_id: 5,
            word_id: MISSING_WORD.to_string(),
            program: None,
            args: vec![],
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(5));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_close_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabClose {
            request_id: 6,
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
        })
        .await;
        assert!(keep);
        // A `TabClosed` reply also suppresses the session-event broadcast that
        // follows it, so the old answer was a success the rest of the fleet
        // never heard about.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(6));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_rename_for_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabRename {
            request_id: 7,
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            new_name: "renamed".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(7));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn tab_reorder_for_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::TabReorder {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            new_position: 1,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        // `TabReorder` carries no request id, so the error cannot correlate.
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn pane_split_in_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneSplit {
            request_id: 8,
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            from_pane: 0,
            dir: SplitDir::Horizontal,
            program: None,
            args: vec![],
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(8));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn pane_swap_in_an_unknown_session_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneSwap {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            a: 0,
            b: 1,
        })
        .await;
        assert!(keep);
        // SUSPECT: the arm discards the `Err` with `if let Ok(..)`, so a swap
        // against a session that does not exist is silently dropped — no layout
        // broadcast and no error reaches the client.
        assert!(msgs.is_empty(), "the failure is swallowed: {msgs:?}");
    }

    #[tokio::test]
    async fn set_layout_ratios_in_an_unknown_session_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetLayoutRatios {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            path: vec![],
            ratios: vec![500, 500],
        })
        .await;
        assert!(keep);
        // SUSPECT: same swallowed `Err` as `PaneSwap`.
        assert!(msgs.is_empty(), "the failure is swallowed: {msgs:?}");
    }

    #[tokio::test]
    async fn apply_layout_scheme_in_an_unknown_session_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::ApplyLayoutScheme {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            scheme: LayoutScheme::EvenHorizontal,
        })
        .await;
        assert!(keep);
        // SUSPECT: same swallowed `Err` as `PaneSwap`.
        assert!(msgs.is_empty(), "the failure is swallowed: {msgs:?}");
    }

    #[tokio::test]
    async fn set_focus_in_an_unknown_session_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetFocus {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            pane_index: 0,
        })
        .await;
        assert!(keep);
        // SUSPECT: same swallowed `Err` as `PaneSwap`.
        assert!(msgs.is_empty(), "the failure is swallowed: {msgs:?}");
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

    #[tokio::test]
    async fn session_restore_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionRestore {
            request_id: 11,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(11));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn process_overview_on_an_empty_server_returns_no_panes() {
        let (keep, msgs) = dispatch_one(ClientMessage::ProcessOverview { request_id: 12 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::ProcessOverviewResult { request_id, panes } => {
                assert_eq!(request_id, 12);
                assert!(panes.is_empty(), "no panes exist: {panes:?}");
            }
            other => panic!("expected ProcessOverviewResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_logs_answers_a_request_correlated_terminated_stream() {
        let (keep, msgs) = dispatch_one(ClientMessage::FetchLogs {
            request_id: 21,
            lines: Some(5),
            follow: false,
        })
        .await;
        assert!(keep);
        // Whether the daemon log file exists depends on the machine's state dir,
        // so the pinned invariants are the ones the arm controls: every reply
        // carries this request id, and the stream is terminated exactly once —
        // by `LogEnd` when the log was readable, by an `Error` when it was not.
        assert!(!msgs.is_empty(), "the arm always answers");
        for msg in &msgs {
            let id = match msg {
                ServerMessage::LogChunk { request_id, .. }
                | ServerMessage::LogEnd { request_id } => Some(*request_id),
                ServerMessage::Error { request_id, .. } => *request_id,
                other => panic!("unexpected FetchLogs reply {other:?}"),
            };
            assert_eq!(id, Some(21), "reply not correlated: {msg:?}");
        }
        match msgs.last().expect("non-empty asserted above") {
            ServerMessage::LogEnd { .. } => {}
            ServerMessage::Error { code, .. } => assert_eq!(*code, ErrorCode::InternalError),
            other => panic!("stream must end with LogEnd or Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pty_input_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyInput {
            pane_id: MISSING_PANE.to_string(),
            data: b"x".to_vec(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn pty_paste_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyPaste {
            pane_id: MISSING_PANE.to_string(),
            data: "x".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn pty_key_batch_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyKeyBatch {
            pane_id: MISSING_PANE.to_string(),
            events: vec![one_key()],
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn an_empty_pty_key_batch_answers_nothing_even_for_an_unknown_pane() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyKeyBatch {
            pane_id: MISSING_PANE.to_string(),
            events: vec![],
        })
        .await;
        assert!(keep);
        // `write_key_batch` short-circuits on an empty batch before the pane
        // lookup, so the pane is never validated.
        assert!(msgs.is_empty(), "nothing to write: {msgs:?}");
    }

    #[tokio::test]
    async fn resize_of_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Resize {
            pane_id: MISSING_PANE.to_string(),
            size: TermSize::default(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn attach_to_an_unknown_pane_errors_and_starts_no_stream() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(
            &mut state,
            ClientMessage::Attach {
                pane_id: MISSING_PANE.to_string(),
                last_seqno: None,
                size: TermSize::default(),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        assert!(
            state.attached.is_empty(),
            "a failed attach registers no forwarding task"
        );
        let (request_id, code, message) = only_error(drain(&mut ctrl_rx));
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn detach_from_a_pane_this_client_never_attached_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::Detach {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        assert!(msgs.is_empty(), "nothing was attached: {msgs:?}");
    }

    #[tokio::test]
    async fn signal_to_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Signal {
            pane_id: MISSING_PANE.to_string(),
            signal: 15,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn request_input_lock_on_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::RequestInputLock {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn release_input_lock_on_an_unknown_pane_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ReleaseInputLock {
            pane_id: MISSING_PANE.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn session_rename_of_an_unknown_session_errors_naming_the_word_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SessionRename {
            request_id: 13,
            word_id: MISSING_WORD.to_string(),
            new_name: "renamed".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(13));
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn set_snapshot_mode_is_applied_without_a_reply() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetSnapshotMode { enabled: true }).await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "snapshot mode is a silent connection setting: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn set_paused_is_applied_without_a_reply() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetPaused {
            paused: true,
            auto: false,
        })
        .await;
        assert!(keep);
        assert!(
            msgs.is_empty(),
            "pausing is a silent connection setting: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn set_pane_no_auto_pause_for_an_unknown_pane_answers_nothing() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetPaneNoAutoPause {
            pane_id: MISSING_PANE.to_string(),
            exempt: true,
        })
        .await;
        assert!(keep);
        // The exemption is a per-client preference, recorded without validating
        // that the pane exists.
        assert!(msgs.is_empty(), "no reply is defined: {msgs:?}");
    }

    #[tokio::test]
    async fn fetch_history_for_an_unknown_pane_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::FetchHistory {
            request_id: 14,
            pane_id: MISSING_PANE.to_string(),
            start_index: 0,
            count: 10,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(14));
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn list_directory_of_a_missing_path_answers_a_listing_carrying_the_error() {
        let (keep, msgs) = dispatch_one(ClientMessage::ListDirectory {
            request_id: 15,
            path: "/this/path/does/not/exist/kmux".to_string(),
        })
        .await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::DirectoryListing {
                request_id,
                path,
                parent,
                entries,
                error,
            } => {
                assert_eq!(request_id, 15);
                // The requested path is echoed back verbatim, not canonicalized.
                assert_eq!(path, "/this/path/does/not/exist/kmux");
                assert_eq!(parent, None);
                assert!(entries.is_empty());
                assert!(error.is_some(), "the IO failure is reported inline");
            }
            other => panic!("expected DirectoryListing, got {other:?}"),
        }
    }

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

    #[tokio::test]
    async fn client_list_for_an_unknown_session_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ClientList {
            request_id: 18,
            word_id: MISSING_WORD.to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(18));
        assert_eq!(code, ErrorCode::SessionNotFound);
        // SUSPECT: unlike the app-layer errors above, this message does not name
        // the word id the client asked about.
        assert_eq!(message, "session not found");
    }

    #[tokio::test]
    async fn kick_client_in_an_unknown_session_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::KickClient {
            request_id: 19,
            word_id: MISSING_WORD.to_string(),
            client_id: ClientId(42),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(19));
        assert_eq!(code, ErrorCode::SessionNotFound);
        // SUSPECT: as with `ClientList`, neither the word id nor the target
        // client id appears in the message.
        assert_eq!(message, "session not found");
    }

    #[tokio::test]
    async fn notify_for_an_unknown_pane_errors_with_the_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::Notify {
            request_id: 20,
            pane_id: MISSING_PANE.to_string(),
            kind: AttentionKind::TurnDone,
            title: "title".to_string(),
            body: "body".to_string(),
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, Some(20));
        // The protocol doc for `Notify` promises an error when "the pane is
        // unknown", and this is the code that says so.
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
    }

    #[tokio::test]
    async fn ping_is_answered_with_a_pong_carrying_the_same_seq() {
        let (keep, msgs) = dispatch_one(ClientMessage::Ping { seq: 7 }).await;
        assert!(keep);
        match only(msgs) {
            ServerMessage::Pong { seq } => assert_eq!(seq, 7),
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unsolicited_pong_answers_nothing_and_records_no_rtt() {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(&mut state, ClientMessage::Pong { seq: 7 }, &NoopAttacher).await;
        assert!(keep);
        assert!(drain(&mut ctrl_rx).is_empty(), "a Pong is not answered");
        // No ping was ever sent, so both samples stay at their initial values:
        // `u64::MAX` is the "no RTT measured yet" sentinel, `0` the "never".
        assert_eq!(state.metrics.last_rtt_ms.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(state.metrics.last_pong_ms.load(Ordering::Relaxed), 0);
    }
}
