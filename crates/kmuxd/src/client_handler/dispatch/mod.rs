//! The client-message router.
//!
//! `handle_message` is a dispatch table and nothing else: every arm
//! destructures its message and calls one named handler in the domain module
//! that owns it. This is not only readability. cargo-mutants generates one
//! body-replacement mutant **per function**, so the 887-line, 43-arm match this
//! replaced produced exactly ONE mutant covering all 43 message types — any
//! single test killed it and the other 42 were invisible to the metric. Forty
//! handlers produce forty mutants, each killable only by an assertion about
//! that message. See docs/testing.md R4 and docs/quality-gates.md.
//!
//! Handlers return `()`, not an effect type: no message a client sends after
//! authenticating can close the connection, so there is nothing for the router
//! to act on. The handshake is the exception and says so in its type — the
//! pre-auth handlers return [`Flow`].
//!
//! The match stays flat and exhaustive rather than delegating to per-domain
//! sub-routers. Exhaustiveness is the property worth keeping: adding a variant
//! to `ClientMessage` fails this build until someone decides what the daemon
//! does with it. That costs a table long enough to still trip
//! `clippy::too_many_lines`, which the ratchet holds at its current count —
//! the lint's job is to stop *logic* accumulating in one function, and there
//! is none here.

mod auth;
mod clients;
mod diagnostics;
mod input;
mod layout;
mod pane;
mod peer;
mod session;
mod tab;
mod view;

use kmux_protocol::messages::{ClientMessage, TermSize};

use super::{PaneAttacher, SharedClientState};

/// What the router does with the connection once a message has been handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep reading from this connection.
    Continue,
    /// Close it. Only the handshake has grounds for this.
    Close,
}

impl Flow {
    /// The `bool` [`handle_message`] answers with: `true` keeps the read loop
    /// going.
    pub(crate) const fn keep_reading(self) -> bool {
        matches!(self, Self::Continue)
    }
}

/// The three spawn parameters four different messages all carry.
///
/// `SessionCreate`, `PaneCreate`, `TabCreate` and `PaneSplit` each say "make me
/// a new PTY" and each carries the same trio to describe it. Passed as one value
/// because positionally they are `Option<String>`, `Vec<String>` and a struct —
/// three arguments a caller can transpose and the compiler cannot catch.
pub(super) struct Spawn {
    /// Program to run; `None` means the configured default shell.
    pub program: Option<String>,
    /// Arguments to `program`.
    pub args: Vec<String>,
    /// Initial terminal size for the new pane.
    pub size: TermSize,
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
        return auth::handle_unauthenticated(state, msg)
            .await
            .keep_reading();
    }

    let client_id = state.client_id.expect("authenticated without client_id");

    match msg {
        ClientMessage::Auth { .. } => {}

        // Already authenticated — a stray proof is ignored.
        ClientMessage::AuthProof { .. } => {}

        ClientMessage::ChannelReady => auth::on_channel_ready(state),

        ClientMessage::SessionCreate {
            request_id,
            name,
            cwd,
            program,
            args,
            size,
            peer,
        } => {
            let spawn = Spawn {
                program,
                args,
                size,
            };
            session::on_session_create(state, request_id, name, cwd, spawn, peer).await;
        }

        ClientMessage::SessionClose {
            request_id,
            word_id,
        } => session::on_session_close(state, client_id, request_id, word_id).await,

        ClientMessage::PaneCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => {
            let spawn = Spawn {
                program,
                args,
                size,
            };
            pane::on_pane_create(state, request_id, word_id, spawn).await;
        }

        ClientMessage::PaneClose {
            request_id,
            pane_id,
        } => pane::on_pane_close(state, client_id, request_id, pane_id).await,

        ClientMessage::TabCreate {
            request_id,
            word_id,
            program,
            args,
            size,
        } => {
            let spawn = Spawn {
                program,
                args,
                size,
            };
            tab::on_tab_create(state, request_id, word_id, spawn).await;
        }

        ClientMessage::TabClose {
            request_id,
            word_id,
            tab_index,
        } => tab::on_tab_close(state, request_id, word_id, tab_index).await,

        ClientMessage::TabRename {
            request_id,
            word_id,
            tab_index,
            new_name,
        } => tab::on_tab_rename(state, request_id, word_id, tab_index, new_name).await,

        ClientMessage::TabReorder {
            word_id,
            tab_index,
            new_position,
        } => tab::on_tab_reorder(state, word_id, tab_index, new_position).await,

        ClientMessage::PaneSplit {
            request_id,
            word_id,
            tab_index,
            from_pane,
            dir,
            program,
            args,
            size,
        } => {
            let spawn = Spawn {
                program,
                args,
                size,
            };
            pane::on_pane_split(state, request_id, word_id, tab_index, from_pane, dir, spawn).await;
        }

        ClientMessage::PaneSwap {
            word_id,
            tab_index,
            a,
            b,
        } => layout::on_pane_swap(state, word_id, tab_index, a, b).await,

        ClientMessage::SetLayoutRatios {
            word_id,
            tab_index,
            path,
            ratios,
        } => layout::on_set_layout_ratios(state, word_id, tab_index, path, ratios).await,

        ClientMessage::ApplyLayoutScheme {
            word_id,
            tab_index,
            scheme,
        } => layout::on_apply_layout_scheme(state, word_id, tab_index, scheme).await,

        ClientMessage::SetFocus {
            word_id,
            tab_index,
            pane_index,
        } => layout::on_set_focus(state, word_id, tab_index, pane_index).await,

        ClientMessage::SessionList { request_id } => {
            session::on_session_list(state, request_id).await;
        }

        ClientMessage::SessionListClosed { request_id } => {
            session::on_session_list_closed(state, request_id);
        }

        ClientMessage::SessionRestore {
            request_id,
            word_id,
        } => session::on_session_restore(state, request_id, word_id).await,

        ClientMessage::ProcessOverview { request_id } => {
            diagnostics::on_process_overview(state, request_id).await;
        }

        ClientMessage::FetchLogs {
            request_id,
            lines,
            follow,
        } => diagnostics::on_fetch_logs(state, request_id, lines, follow).await,

        ClientMessage::PtyInput { pane_id, data } => {
            input::on_pty_input(state, client_id, pane_id, data).await;
        }

        ClientMessage::PtyPaste { pane_id, data } => {
            input::on_pty_paste(state, client_id, pane_id, data).await;
        }

        ClientMessage::PtyKeyBatch { pane_id, events } => {
            input::on_pty_key_batch(state, client_id, pane_id, events).await;
        }

        ClientMessage::Resize { pane_id, size } => {
            input::on_resize(state, client_id, pane_id, size).await;
        }

        ClientMessage::Attach {
            pane_id,
            last_seqno,
            size,
        } => view::on_attach(state, client_id, pane_id, last_seqno, size, attacher).await,

        ClientMessage::Detach { pane_id } => view::on_detach(state, client_id, pane_id).await,

        ClientMessage::Signal { pane_id, signal } => input::on_signal(state, pane_id, signal).await,

        ClientMessage::RequestInputLock { pane_id } => {
            input::on_request_input_lock(state, client_id, pane_id).await;
        }

        ClientMessage::ReleaseInputLock { pane_id } => {
            input::on_release_input_lock(state, client_id, pane_id).await;
        }

        ClientMessage::SessionRename {
            request_id,
            word_id,
            new_name,
        } => session::on_session_rename(state, request_id, word_id, new_name).await,

        ClientMessage::SetSnapshotMode { enabled } => {
            view::on_set_snapshot_mode(state, client_id, enabled).await;
        }

        ClientMessage::SetPaused { paused, auto } => {
            view::on_set_paused(state, client_id, paused, auto).await;
        }

        ClientMessage::SetPaneNoAutoPause { pane_id, exempt } => {
            view::on_set_pane_no_auto_pause(state, client_id, pane_id, exempt).await;
        }

        ClientMessage::FetchHistory {
            request_id,
            pane_id,
            start_index,
            count,
        } => view::on_fetch_history(state, request_id, pane_id, start_index, count).await,

        ClientMessage::ListDirectory { request_id, path } => {
            diagnostics::on_list_directory(state, request_id, &path);
        }

        ClientMessage::OpenPeer { request_id, target } => {
            peer::on_open_peer(state, request_id, target).await;
        }

        ClientMessage::ClosePeer { request_id, peer } => {
            peer::on_close_peer(state, request_id, peer);
        }

        ClientMessage::ClientList {
            request_id,
            word_id,
        } => clients::on_client_list(state, client_id, request_id, word_id).await,

        ClientMessage::KickClient {
            request_id,
            word_id,
            client_id: target,
        } => clients::on_kick_client(state, client_id, request_id, word_id, target).await,

        ClientMessage::Notify {
            request_id,
            pane_id,
            kind,
            title,
            body,
        } => diagnostics::on_notify(state, request_id, pane_id, kind, title, body).await,

        ClientMessage::Ping { seq } => diagnostics::on_ping(state, seq),

        ClientMessage::Pong { seq } => diagnostics::on_pong(state, seq),
    }

    true
}

#[cfg(test)]
pub(super) mod testing {
    //! Shared fixtures for the per-arm characterization tests.
    //!
    //! Every domain module's tests drive the real router (`handle_message`) against
    //! a real `ServerApp`, so what they pin is what a client would see. These are
    //! the pieces that would otherwise be copied ten times.

    //! Everything below is re-exported, so a domain module's tests need exactly
    //! one `use super::super::testing::*;` and no per-file import drift.

    pub(super) use std::sync::Arc;
    pub(super) use std::sync::atomic::Ordering;

    pub(super) use kmux_protocol::messages::{
        AttentionKind, ClientCapabilities, ClientId, ClientMessage, Compression, ErrorCode,
        KeyAction, KeyCode, KeyEvent, KeyMods, LayoutScheme, PROTOCOL_RANGE, PeerTarget,
        ProtocolRange, ProtocolVersion, ServerMessage, SplitDir, TermSize, protocol_capabilities,
    };
    pub(super) use kmux_protocol::{Compressor, TransportKind};
    pub(super) use tokio::sync::mpsc;
    pub(super) use tokio::task::AbortHandle;

    pub(super) use crate::app::{AttachResult, ConnectionMetrics, ServerApp};
    pub(super) use crate::client_handler::{OutboundCompression, PaneAttacher, SharedClientState};
    pub(super) use crate::config::{CompressionConfig, CompressionMode};

    pub(super) use super::handle_message;

    /// A word id no session uses, so every session-scoped arm takes its
    /// not-found path.
    pub(super) const MISSING_WORD: &str = "nosuch";
    /// A well-formed pane id (`word/index`) that parses but resolves to nothing.
    pub(super) const MISSING_PANE: &str = "nosuch/0";

    /// Auth doesn't attach panes, so a never-called stub attacher suffices.
    pub(super) struct NoopAttacher;

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

    pub(super) fn state_for(
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

    pub(super) async fn authenticate_with_capabilities(
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

    pub(super) async fn authenticate(state: &mut SharedClientState) {
        authenticate_with_capabilities(state, protocol_capabilities()).await;
    }

    /// An authenticated client on an empty server, with the handshake replies
    /// already drained so an assertion sees only the arm under test.
    pub(super) async fn authenticated_client()
    -> (SharedClientState, mpsc::UnboundedReceiver<ServerMessage>) {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let (mut state, _comp_out, mut ctrl_rx) = state_for(app, TransportKind::Uds);
        authenticate(&mut state).await;
        while ctrl_rx.try_recv().is_ok() {}
        (state, ctrl_rx)
    }

    /// Everything queued on the control channel, in order.
    pub(super) fn drain(
        ctrl_rx: &mut mpsc::UnboundedReceiver<ServerMessage>,
    ) -> Vec<ServerMessage> {
        let mut out = Vec::new();
        while let Ok(msg) = ctrl_rx.try_recv() {
            out.push(msg);
        }
        out
    }

    /// Dispatch exactly one message to a freshly authenticated client on an
    /// empty server; returns the keep-reading flag and everything it emitted.
    pub(super) async fn dispatch_one(msg: ClientMessage) -> (bool, Vec<ServerMessage>) {
        let (mut state, mut ctrl_rx) = authenticated_client().await;
        let keep = handle_message(&mut state, msg, &NoopAttacher).await;
        let out = drain(&mut ctrl_rx);
        (keep, out)
    }

    /// The single message an arm emitted; panics when it emitted zero or many.
    pub(super) fn only(msgs: Vec<ServerMessage>) -> ServerMessage {
        assert_eq!(msgs.len(), 1, "expected exactly one reply, got {msgs:?}");
        msgs.into_iter().next().expect("length asserted above")
    }

    /// The parts of the single `Error` an arm emitted.
    pub(super) fn only_error(msgs: Vec<ServerMessage>) -> (Option<u64>, ErrorCode, String) {
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
    pub(super) fn one_key() -> KeyEvent {
        KeyEvent {
            code: KeyCode::A,
            mods: KeyMods::empty(),
            action: KeyAction::Press,
            text: "a".to_string(),
            unshifted_codepoint: u32::from('a'),
        }
    }

    /// A session running one long-lived childless process, plus an authenticated
    /// client on the same `ServerApp`. For the arms that need something to exist.
    pub(super) async fn app_with_one_session() -> (
        Arc<ServerApp>,
        String,
        SharedClientState,
        mpsc::UnboundedReceiver<ServerMessage>,
    ) {
        let app = Arc::new(ServerApp::new("tok".to_string()));
        let entry = app
            .create_session(
                None,
                Some("/tmp".to_string()),
                Some("/bin/sleep".to_string()),
                vec!["30".to_string()],
                TermSize::default(),
                &ClientCapabilities::default(),
            )
            .await
            .expect("create_session");
        let word = entry.meta.word_id;
        let (mut state, _comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        authenticate(&mut state).await;
        while ctrl_rx.try_recv().is_ok() {}
        (app, word, state, ctrl_rx)
    }
}
