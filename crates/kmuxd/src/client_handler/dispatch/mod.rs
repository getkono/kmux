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
mod tests {
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use kmux_protocol::messages::ErrorCode;

    use super::diagnostics::list_directory;

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
    // docs/testing.md R2 and R4. Every expectation was first read off the
    // running code rather than invented, and where it looked wrong it was
    // recorded faithfully and marked `// SUSPECT:` rather than "corrected"
    // here — a characterization commit that changes behaviour characterizes
    // nothing. The five findings that produced were then fixed in five
    // `fix(kmuxd):` commits of their own, each flipping its assertion here;
    // `git log --grep "^fix(kmuxd)" -- .` is the list.

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
    async fn pane_swap_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::PaneSwap {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            a: 0,
            b: 1,
        })
        .await;
        assert!(keep);
        // No `request_id` on the wire for the four layout nudges, so the error
        // is unaddressed — the shape `Resize` and `Signal` already use.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    /// The other half of the four layout arms: a call that succeeds must still
    /// broadcast the daemon's new authoritative tree and send the requester
    /// nothing. Without this, an arm that reported every outcome as an error
    /// would pass the four not-found tests above.
    #[tokio::test]
    async fn a_layout_change_that_succeeds_broadcasts_and_answers_nothing() {
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
        let word = entry.meta.word_id.clone();

        let mut events = app.subscribe_vt_events();
        let (mut state, _comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        authenticate(&mut state).await;
        while ctrl_rx.try_recv().is_ok() {}

        let keep = handle_message(
            &mut state,
            ClientMessage::SetFocus {
                word_id: word.clone(),
                tab_index: 0,
                pane_index: 0,
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        assert!(
            drain(&mut ctrl_rx).is_empty(),
            "a successful layout change is answered by the broadcast, not a reply"
        );
        match events.try_recv().expect("a LayoutUpdate was broadcast") {
            ServerMessage::LayoutUpdate {
                word_id,
                tab_index,
                focused_pane,
                ..
            } => {
                assert_eq!(word_id, word);
                assert_eq!(tab_index, 0);
                assert_eq!(focused_pane, 0);
            }
            other => panic!("expected LayoutUpdate, got {other:?}"),
        }

        let _ = app.close_session(&word).await;
    }

    #[tokio::test]
    async fn set_layout_ratios_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetLayoutRatios {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            path: vec![],
            ratios: vec![500, 500],
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn apply_layout_scheme_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::ApplyLayoutScheme {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            scheme: LayoutScheme::EvenHorizontal,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    #[tokio::test]
    async fn set_focus_in_an_unknown_session_errors_without_a_request_id() {
        let (keep, msgs) = dispatch_one(ClientMessage::SetFocus {
            word_id: MISSING_WORD.to_string(),
            tab_index: 0,
            pane_index: 0,
        })
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::SessionNotFound);
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
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
    async fn an_empty_pty_key_batch_to_an_unknown_pane_still_errors() {
        let (keep, msgs) = dispatch_one(ClientMessage::PtyKeyBatch {
            pane_id: MISSING_PANE.to_string(),
            events: vec![],
        })
        .await;
        assert!(keep);
        // Whether the pane exists cannot depend on how many keys were sent.
        let (request_id, code, message) = only_error(msgs);
        assert_eq!(request_id, None);
        assert_eq!(code, ErrorCode::PaneNotFound);
        assert_eq!(message, format!("pane not found: {MISSING_PANE}"));
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
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
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
        assert_eq!(message, format!("session not found: {MISSING_WORD}"));
    }

    /// The other `KickClient` failure: the session is real, the client id is
    /// not attached to it. Its message names both, so a caller looking at a log
    /// line can tell which of the two was wrong.
    #[tokio::test]
    async fn kicking_a_client_that_is_not_attached_names_the_client_and_the_session() {
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
        let word = entry.meta.word_id.clone();

        let (mut state, _comp_out, mut ctrl_rx) = state_for(Arc::clone(&app), TransportKind::Uds);
        authenticate(&mut state).await;
        while ctrl_rx.try_recv().is_ok() {}

        let keep = handle_message(
            &mut state,
            ClientMessage::KickClient {
                request_id: 21,
                word_id: word.clone(),
                client_id: ClientId(42),
            },
            &NoopAttacher,
        )
        .await;
        assert!(keep);
        let (request_id, code, message) = only_error(drain(&mut ctrl_rx));
        assert_eq!(request_id, Some(21));
        assert_eq!(code, ErrorCode::ClientNotFound);
        assert_eq!(message, format!("client 42 not attached to session {word}"));

        let _ = app.close_session(&word).await;
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
