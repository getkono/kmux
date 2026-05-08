use std::sync::atomic::Ordering;

use kmux_protocol::messages::{ClientMessage, ErrorCode, ServerMessage, epoch_millis};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::app::{AttachParams, InputLockOutcome};
use crate::auth::validate_token;
use crate::connection::classify_error;

use super::{CLIENT_CHANNEL_CAPACITY, PaneAttacher, SharedClientState};

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
        if let ClientMessage::Auth {
            token,
            protocol_version,
            capabilities,
            connection_id: incoming_conn_id,
        } = msg
        {
            if protocol_version != kmux_protocol::messages::PROTOCOL_VERSION {
                state.send(ServerMessage::AuthResult {
                    success: false,
                    reason: Some(format!(
                        "protocol version mismatch: client={protocol_version}, server={}",
                        kmux_protocol::messages::PROTOCOL_VERSION
                    )),
                    client_id: None,
                    server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    connection_id: None,
                });
                warn!(
                    "Protocol version mismatch: client={protocol_version}, server={}",
                    kmux_protocol::messages::PROTOCOL_VERSION
                );
                return false;
            } else if validate_token(&token, &state.app.auth_token) {
                let (client_id, conn_id, _metrics, previous_transport) = state
                    .app
                    .register_client(
                        state.transport,
                        std::sync::Arc::clone(&state.metrics),
                        incoming_conn_id,
                    )
                    .await;
                state.client_id = Some(client_id);
                state.connection_id = Some(conn_id);
                state.capabilities = capabilities;
                state.authenticated = true;
                state.pending_swap_from = previous_transport;
                state.conn_span.record("conn_id", conn_id.0);
                state.conn_span.record("client_id", client_id.0);
                state.send(ServerMessage::AuthResult {
                    success: true,
                    reason: None,
                    client_id: Some(client_id),
                    server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    connection_id: Some(conn_id),
                });
                info!(
                    conn_id = conn_id.0,
                    client_id = client_id.0,
                    "client authenticated"
                );
            } else {
                state.send(ServerMessage::AuthResult {
                    success: false,
                    reason: Some("invalid token".to_string()),
                    client_id: None,
                    server_version: None,
                    connection_id: None,
                });
                warn!("authentication failed");
            }
        } else {
            state.error(None, ErrorCode::NotAuthenticated, "send Auth first");
        }
        return true;
    }

    let client_id = state.client_id.expect("authenticated without client_id");

    match msg {
        ClientMessage::Auth { .. } => {}

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
        } => match state
            .app
            .create_session(name, cwd, program, args, size, &state.capabilities)
            .await
        {
            Ok(entry) => state.send(ServerMessage::SessionCreated { request_id, entry }),
            Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
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

        ClientMessage::PaneCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => match state
            .app
            .create_pane(&word_id, program, args, size, &state.capabilities)
            .await
        {
            Ok(pane_id) => state.send(ServerMessage::PaneCreated {
                request_id,
                pane_id,
                session_word_id: word_id,
                size,
            }),
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
                Ok(exit_code) => state.send(ServerMessage::PaneClosed {
                    request_id,
                    pane_id,
                    exit_code,
                }),
                Err(e) => state.error(Some(request_id), classify_error(&e), e.to_string()),
            }
        }

        ClientMessage::SessionList { request_id } => {
            let sessions = state.app.list_sessions().await;
            state.send(ServerMessage::SessionListResult {
                request_id,
                sessions,
            });
        }

        ClientMessage::PtyInput { pane_id, data } => {
            if let Err(e) = state.app.write_input(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::PtyPaste { pane_id, data } => {
            if let Err(e) = state.app.write_paste(&pane_id, client_id, data).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Resize { pane_id, size } => {
            if let Err(e) = state.app.resize(&pane_id, client_id, size).await {
                state.error(None, classify_error(&e), e.to_string());
            }
        }

        ClientMessage::Attach {
            pane_id,
            last_seqno,
            size,
        } => {
            // If already attached, detach first.
            if let Some(old) = state.attached.remove(&pane_id) {
                old.abort();
                state.app.detach_from_pane(&pane_id, client_id).await;
            }

            let (client_tx, client_rx) = mpsc::channel::<ServerMessage>(CLIENT_CHANNEL_CAPACITY);

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

        ClientMessage::Detach { pane_id } => {
            if let Some(handle) = state.attached.remove(&pane_id) {
                handle.abort();
                state.app.detach_from_pane(&pane_id, client_id).await;
                debug!("detached from pane '{pane_id}'");
            }
        }

        ClientMessage::Signal { pane_id, signal } => {
            if let Err(e) = state.app.send_signal(&pane_id, signal).await {
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

        ClientMessage::FetchHistory {
            request_id,
            pane_id,
            start_index,
            count,
        } => match state.app.fetch_history(&pane_id, start_index, count).await {
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
