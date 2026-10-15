//! What a federation hub does with each client message (issue #227).
//!
//! A hub proxies its peers' sessions to its own clients
//! (`docs/architecture-federation.md`), so every message a client can send
//! has one disposition for a session the hub proxies — the **Federated**
//! column of `docs/protocol.md`'s client catalogue, which the `spec` tests
//! hold to [`ClientMessage::federation`]. The match there is exhaustive: a
//! new message does not build until it has one.

use super::client::ClientMessage;
use super::session::{PaneId, PeerId, RequestId, WordId};

/// What a federation hub does with a message when the session it names is
/// proxied from a peer.
#[derive(Debug, PartialEq, Eq)]
pub enum Federation<'a> {
    /// The hub forwards it to the peer, under the peer's ids and an id of its
    /// own, and sends the peer's answer back to the sender alone, under the
    /// sender's ids. The handles are mutable so the hub rewrites them in
    /// place.
    Forward {
        /// The session (or pane, or peer) the message names.
        target: Target<'a>,
        /// The message's `request_id`, when it has one.
        request_id: Option<&'a mut RequestId>,
    },
    /// Names a pane the hub keeps state for per viewer (the attachment, the
    /// size, the input lock): the hub combines its viewers' messages into
    /// what it sends the peer.
    Aggregate,
    /// About the connection, or the hub itself, or every session at once: the
    /// hub answers it and sends the peer nothing.
    Hub,
}

/// The session a forwarded message names, as a handle the hub rewrites.
#[derive(Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// A session, by its word.
    Session(&'a mut WordId),
    /// A pane, by `word/index`.
    Pane(&'a mut PaneId),
    /// A peer, for a session that does not exist yet (`SessionCreate` with a
    /// `peer`). The hub clears it: the peer creates the session on itself.
    Peer(&'a mut Option<PeerId>),
}

impl ClientMessage {
    /// This message's federation disposition — see [`Federation`].
    pub fn federation(&mut self) -> Federation<'_> {
        use Federation::{Aggregate, Hub};
        match self {
            Self::SessionCreate {
                request_id, peer, ..
            }
            | Self::SessionRestore {
                request_id, peer, ..
            } => on_peer(peer, request_id),
            Self::SessionClose {
                request_id,
                word_id,
            }
            | Self::SessionRename {
                request_id,
                word_id,
                ..
            }
            | Self::PaneCreate {
                request_id,
                word_id,
                ..
            }
            | Self::TabCreate {
                request_id,
                word_id,
                ..
            }
            | Self::TabClose {
                request_id,
                word_id,
                ..
            }
            | Self::TabRename {
                request_id,
                word_id,
                ..
            }
            | Self::PaneSplit {
                request_id,
                word_id,
                ..
            }
            | Self::ClientList {
                request_id,
                word_id,
            }
            | Self::KickClient {
                request_id,
                word_id,
                ..
            } => forward(Target::Session(word_id), Some(request_id)),
            Self::TabReorder { word_id, .. }
            | Self::PaneSwap { word_id, .. }
            | Self::SetLayoutRatios { word_id, .. }
            | Self::ApplyLayoutScheme { word_id, .. }
            | Self::SetFocus { word_id, .. } => forward(Target::Session(word_id), None),
            Self::PaneClose {
                request_id,
                pane_id,
            }
            | Self::FetchHistory {
                request_id,
                pane_id,
                ..
            }
            | Self::Notify {
                request_id,
                pane_id,
                ..
            } => forward(Target::Pane(pane_id), Some(request_id)),
            Self::PtyInput { pane_id, .. }
            | Self::PtyKeyBatch { pane_id, .. }
            | Self::PtyPaste { pane_id, .. }
            | Self::Signal { pane_id, .. } => forward(Target::Pane(pane_id), None),
            Self::Attach { .. }
            | Self::Detach { .. }
            | Self::Resize { .. }
            | Self::RequestInputLock { .. }
            | Self::ReleaseInputLock { .. } => Aggregate,
            Self::Auth { .. }
            | Self::AuthProof { .. }
            | Self::ChannelReady
            | Self::SessionList { .. }
            | Self::SessionListClosed { .. }
            | Self::ProcessOverview { .. }
            | Self::SetSnapshotMode { .. }
            | Self::SetPaused { .. }
            | Self::SetPaneNoAutoPause { .. }
            | Self::Ping { .. }
            | Self::Pong { .. }
            | Self::ListDirectory { .. }
            | Self::OpenPeer { .. }
            | Self::ClosePeer { .. }
            | Self::FetchLogs { .. } => Hub,
        }
    }
}

/// A create or a restore: forwarded to the peer it names, the hub's own when
/// it names none.
fn on_peer<'a>(peer: &'a mut Option<PeerId>, request_id: &'a mut RequestId) -> Federation<'a> {
    if peer.is_some() {
        forward(Target::Peer(peer), Some(request_id))
    } else {
        Federation::Hub
    }
}

fn forward<'a>(target: Target<'a>, request_id: Option<&'a mut RequestId>) -> Federation<'a> {
    Federation::Forward { target, request_id }
}

/// One sample of every [`ClientMessage`] variant, for the tests that hold a
/// per-variant property (its category, its federation disposition) to the
/// code. `spec` checks the list names every variant exactly once, so a new
/// variant fails the tests until it has a sample here — and so a documented
/// disposition. `SessionCreate` and `SessionRestore` name a peer: that is
/// the case a hub forwards.
#[cfg(test)]
pub(crate) fn every_client_message() -> Vec<ClientMessage> {
    use super::session::{
        AttentionKind, ClientCapabilities, ClientId, FrontendKind, LayoutScheme, PeerTarget,
        SplitDir, TermSize,
    };
    use super::types::{PROTOCOL_RANGE, protocol_capabilities};
    let size = TermSize::default();
    let word = || "w".to_string();
    let pane = || "w/0".to_string();
    vec![
        ClientMessage::Auth {
            token: "t".into(),
            protocol_range: PROTOCOL_RANGE,
            protocol_capabilities: protocol_capabilities(),
            capabilities: ClientCapabilities::default(),
            connection_id: None,
            resume_instance: None,
            public_key: Vec::new(),
            hostname: String::new(),
            username: String::new(),
            client_kind: FrontendKind::Cli,
            client_git_sha: String::new(),
            client_git_dirty: false,
            client_build_profile: String::new(),
        },
        ClientMessage::AuthProof {
            signature: Vec::new(),
        },
        ClientMessage::ChannelReady,
        ClientMessage::SessionCreate {
            request_id: 0,
            name: None,
            cwd: None,
            program: None,
            args: vec![],
            size,
            peer: Some("box".into()),
        },
        ClientMessage::SessionClose {
            request_id: 0,
            word_id: word(),
        },
        ClientMessage::SessionList { request_id: 0 },
        ClientMessage::ProcessOverview { request_id: 0 },
        ClientMessage::SessionRename {
            request_id: 0,
            word_id: word(),
            new_name: "n".into(),
        },
        ClientMessage::SessionListClosed { request_id: 0 },
        ClientMessage::SessionRestore {
            request_id: 0,
            word_id: word(),
            peer: Some("box".into()),
        },
        ClientMessage::PaneCreate {
            request_id: 0,
            word_id: word(),
            program: None,
            args: vec![],
            size,
        },
        ClientMessage::PaneClose {
            request_id: 0,
            pane_id: pane(),
        },
        ClientMessage::TabCreate {
            request_id: 0,
            word_id: word(),
            program: None,
            args: vec![],
            size,
        },
        ClientMessage::TabClose {
            request_id: 0,
            word_id: word(),
            tab_index: 0,
        },
        ClientMessage::TabRename {
            request_id: 0,
            word_id: word(),
            tab_index: 0,
            new_name: "n".into(),
        },
        ClientMessage::TabReorder {
            word_id: word(),
            tab_index: 0,
            new_position: 1,
        },
        ClientMessage::PaneSplit {
            request_id: 0,
            word_id: word(),
            tab_index: 0,
            from_pane: 0,
            dir: SplitDir::Horizontal,
            program: None,
            args: vec![],
            size,
        },
        ClientMessage::PaneSwap {
            word_id: word(),
            tab_index: 0,
            a: 0,
            b: 1,
        },
        ClientMessage::SetLayoutRatios {
            word_id: word(),
            tab_index: 0,
            path: vec![],
            ratios: vec![500, 500],
        },
        ClientMessage::ApplyLayoutScheme {
            word_id: word(),
            tab_index: 0,
            scheme: LayoutScheme::MainVertical,
        },
        ClientMessage::SetFocus {
            word_id: word(),
            tab_index: 0,
            pane_index: 0,
        },
        ClientMessage::PtyInput {
            pane_id: pane(),
            data: vec![b'a'],
        },
        ClientMessage::PtyKeyBatch {
            pane_id: pane(),
            events: vec![],
        },
        ClientMessage::PtyPaste {
            pane_id: pane(),
            data: "x".into(),
        },
        ClientMessage::Resize {
            pane_id: pane(),
            size,
        },
        ClientMessage::Attach {
            pane_id: pane(),
            last_seqno: None,
            size,
        },
        ClientMessage::Detach { pane_id: pane() },
        ClientMessage::Signal {
            pane_id: pane(),
            signal: 15,
        },
        ClientMessage::RequestInputLock { pane_id: pane() },
        ClientMessage::ReleaseInputLock { pane_id: pane() },
        ClientMessage::SetSnapshotMode { enabled: false },
        ClientMessage::SetPaused {
            paused: true,
            auto: false,
        },
        ClientMessage::SetPaneNoAutoPause {
            pane_id: pane(),
            exempt: true,
        },
        ClientMessage::FetchHistory {
            request_id: 0,
            pane_id: pane(),
            start_index: 0,
            count: 10,
        },
        ClientMessage::Ping { seq: 1 },
        ClientMessage::Pong { seq: 1 },
        ClientMessage::ListDirectory {
            request_id: 0,
            path: "/tmp".into(),
        },
        ClientMessage::OpenPeer {
            request_id: 0,
            target: PeerTarget::Ssh {
                user: None,
                host: "box".into(),
                ssh_port: None,
                accept_invalid_certs: false,
            },
        },
        ClientMessage::ClosePeer {
            request_id: 0,
            peer: "box".into(),
        },
        ClientMessage::ClientList {
            request_id: 0,
            word_id: word(),
        },
        ClientMessage::KickClient {
            request_id: 0,
            word_id: word(),
            client_id: ClientId(1),
        },
        ClientMessage::Notify {
            request_id: 0,
            pane_id: pane(),
            kind: AttentionKind::TurnDone,
            title: String::new(),
            body: String::new(),
        },
        ClientMessage::FetchLogs {
            request_id: 0,
            lines: None,
            follow: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forwarded message hands out its own fields, so a hub's rewrite lands
    /// in the message it sends on.
    #[test]
    fn a_forwarded_message_is_rewritten_in_place() {
        let mut msg = ClientMessage::PaneClose {
            request_id: 7,
            pane_id: "hawk/2".into(),
        };
        let Federation::Forward {
            target: Target::Pane(pane_id),
            request_id: Some(request_id),
        } = msg.federation()
        else {
            panic!("PaneClose is forwarded by pane");
        };
        *pane_id = "eagle/2".into();
        *request_id = 40;
        assert!(matches!(
            msg,
            ClientMessage::PaneClose { request_id: 40, pane_id } if pane_id == "eagle/2"
        ));
    }

    /// Only a create or a restore that names a peer is forwarded (issue
    /// #228); one without is the hub's own.
    #[test]
    fn a_create_or_restore_is_forwarded_only_when_it_names_a_peer() {
        let named_a_peer: Vec<ClientMessage> = every_client_message()
            .into_iter()
            .filter(|m| {
                matches!(
                    m,
                    ClientMessage::SessionCreate { .. } | ClientMessage::SessionRestore { .. }
                )
            })
            .collect();
        assert_eq!(named_a_peer.len(), 2, "a create and a restore");
        for mut msg in named_a_peer {
            let Federation::Forward {
                target: Target::Peer(peer),
                ..
            } = msg.federation()
            else {
                panic!("naming a peer, it is forwarded to it: {msg:?}");
            };
            assert_eq!(peer.take().as_deref(), Some("box"));
            assert_eq!(msg.federation(), Federation::Hub);
        }
    }
}
