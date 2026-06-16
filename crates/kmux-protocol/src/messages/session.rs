use serde::{Deserialize, Serialize};

pub type RequestId = u64;

/// Opaque connection identity assigned by the server on first authentication.
///
/// Survives transport switches: when a client re-authenticates on a new channel
/// (QUIC ↔ TCP) it passes its `ConnectionId` so the server can transfer all
/// pane attachments to the new transport without the client needing to re-attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub u64);

/// Unique word-based session identifier (a single word from the EFF long wordlist).
/// Example: `"eagle"`, `"falcon"`.
pub type WordId = String;

/// Pane identifier: `"{word_id}/{pane_index}"`.
/// Example: `"eagle/0"`, `"eagle/1"`.
pub type PaneId = String;

/// Tab index within a session (0-based, monotonically increasing per session).
///
/// A *tab* is a named tiling layout over a subset of the session's panes. The
/// hierarchy is **Session → Tab → Pane**: a session owns a flat pool of panes
/// (each one PTY, identified by [`PaneId`]) and one or more tabs, each of which
/// arranges some of those panes in a [`LayoutNode`] tree. Tab indices appear
/// only in tab/layout control messages, never in the hot PTY path (which keys
/// off [`PaneId`]).
pub type TabIndex = u32;

/// Rendering capabilities self-declared by a client at Auth time.
///
/// The daemon uses these to decide which PTY environment variables to set
/// for spawned shells and which features to enable in the server-side VT
/// emulator (currently libghostty-vt).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    /// Client can render kitty graphics protocol image data.
    pub kitty_graphics: bool,
    /// Client can encode keyboard input using the kitty keyboard protocol.
    pub kitty_keyboard: bool,
    /// Client can display 24-bit (truecolor) RGB cells directly.
    /// The daemon always sets `COLORTERM=truecolor` today, but this field
    /// is reserved for future per-client downgrade in the forwarding layer.
    pub truecolor: bool,
    /// Client's native host `$TERM` (informational; not used for `TERM` selection).
    pub term: Option<String>,
    /// Client's self-reported `$TERM_PROGRAM` (informational).
    pub term_program: Option<String>,
}

/// Opaque client identity assigned by the server on successful authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClientId(pub u64);

/// Stable identifier for a federated peer daemon (issue #121), derived from its
/// [`PeerTarget`] via [`PeerTarget::peer_id`]. Used to address a peer in
/// `ClosePeer` and to label the sessions it contributes.
pub type PeerId = String;

/// Addressing for a remote `kmuxd` the local daemon should federate to
/// (issue #121). The local daemon opens **one** upstream connection per distinct
/// `PeerTarget` and proxies that peer's sessions to local GUIs. The daemon maps
/// this onto `kmux_connect`'s connect mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerTarget {
    /// Reach the peer over SSH: `kmuxd probe-or-start` over SSH, then TCP+TLS
    /// through the resulting `-L` tunnel. The default for `--server user@host`.
    Ssh {
        /// SSH user (e.g. `alice` in `alice@box`); `None` uses the SSH default.
        user: Option<String>,
        /// Remote host: a hostname, IP, or `~/.ssh/config` alias.
        host: String,
        /// SSH port override; `None` = the SSH default (usually 22).
        ssh_port: Option<u16>,
        /// Accept a self-signed / unpinned TLS certificate on the data plane.
        accept_invalid_certs: bool,
    },
    /// Reach the peer directly over TCP+TLS at `host:port` with a shared `token`.
    /// For LAN peers and same-host multi-daemon setups (including tests) where the
    /// remote `kmuxd` is already listening and SSH is unnecessary.
    Direct {
        /// Remote host (hostname or IP) the remote `kmuxd` listens on.
        host: String,
        /// TCP+TLS port the remote `kmuxd` listens on.
        port: u16,
        /// Shared auth token for the remote daemon.
        token: String,
        /// Accept a self-signed / unpinned TLS certificate on the data plane.
        accept_invalid_certs: bool,
    },
}

impl PeerTarget {
    /// The stable [`PeerId`] for this target: SSH → `"user@host"` (suffixed
    /// `":port"` when `ssh_port` is set, bare `"host"` when no user is given);
    /// Direct → `"host:port"`. Cert policy and token are excluded — they are
    /// policy/credentials, not identity (the same endpoint is one peer).
    pub fn peer_id(&self) -> PeerId {
        match self {
            PeerTarget::Ssh {
                user,
                host,
                ssh_port,
                ..
            } => {
                let base = match user {
                    Some(u) => format!("{u}@{host}"),
                    None => host.clone(),
                };
                match ssh_port {
                    Some(p) => format!("{base}:{p}"),
                    None => base,
                }
            }
            PeerTarget::Direct { host, port, .. } => format!("{host}:{port}"),
        }
    }

    /// Whether to accept a self-signed / unpinned TLS certificate on the data plane.
    pub fn accept_invalid_certs(&self) -> bool {
        match self {
            PeerTarget::Ssh {
                accept_invalid_certs,
                ..
            }
            | PeerTarget::Direct {
                accept_invalid_certs,
                ..
            } => *accept_invalid_certs,
        }
    }
}

/// Monotonic sequence number attached to each PTY output chunk per pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SequenceNo(pub u64);

/// Terminal dimensions (rows × columns × optional pixel extent).
///
/// `pixel_width` and `pixel_height` represent the total drawable area of the
/// terminal window in physical pixels.  A value of `0` means the client does
/// not know (or the platform does not expose) the pixel dimensions — backends
/// must treat `0` as "unknown" and fall back to cell-only sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
    /// Total window width in physical pixels; `0` = unknown.
    pub pixel_width: u16,
    /// Total window height in physical pixels; `0` = unknown.
    pub pixel_height: u16,
}

impl Default for TermSize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// Whether a PTY child process is still running or has exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// Immutable session-level metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Chronological creation index (0-based, monotonically increasing).
    pub index: u32,
    /// Unique word-based identifier (e.g. `"eagle"`).
    pub word_id: WordId,
    /// Human-readable display name (default: `basename(cwd)`).
    pub name: String,
    /// Server-side working directory associated with this session.
    pub cwd: String,
}

/// One entry returned by a `ListDirectory` directory browse.
///
/// Today the daemon only returns subdirectories (the browser chooses a
/// *directory* in which to open a new session), so `is_dir` is always `true`.
/// The field is kept explicit so the wire shape can later carry files too
/// without a breaking change to call sites that already pattern-match on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// The entry's file name (a single path component, not a full path).
    pub name: String,
    /// Whether the entry is a directory. Always `true` in the current protocol.
    pub is_dir: bool,
}

/// Snapshot of a single pane within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    /// Full pane identifier: `"{word_id}/{pane_index}"`.
    pub pane_id: PaneId,
    /// Zero-based index within the session (monotonically increasing per session).
    pub pane_index: u32,
    /// Shell or program running inside this pane.
    pub program: String,
    pub size: TermSize,
    /// IDs of currently attached clients.
    pub attached_clients: Vec<ClientId>,
    /// Whether the pane's child process is still running.
    pub status: SessionStatus,
    /// Latest window title reported by the pane's program via OSC 0/2.
    /// Empty until the program emits a title sequence.
    pub title: String,
}

/// Orientation of a layout split.
///
/// `Horizontal` lays children out **left ↔ right** (a vertical divider between
/// them); `Vertical` lays children **top ↕ bottom** (a horizontal divider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// A preset tiling arrangement the server regenerates a tab's [`LayoutNode`] tree
/// into from its current set of panes (in their existing leaf order), à la tmux's
/// preset layouts. Used by `ApplyLayoutScheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutScheme {
    /// All panes in a single row (one horizontal split).
    EvenHorizontal,
    /// All panes in a single column (one vertical split).
    EvenVertical,
    /// A large "main" pane on the left; the rest stacked in a column on the right.
    MainVertical,
    /// A large "main" pane on top; the rest in a row along the bottom.
    MainHorizontal,
}

/// A resolution-independent tiling layout for one tab.
///
/// Leaves reference a pane by its session-local `pane_index`; `Split` nodes hold
/// child weights as **permille** integers (0..=1000 summing to ~1000), so the
/// tree is bit-exact across clients — every client resolves the *same* tree
/// against *its own* window into per-pane cell rectangles (see the
/// `kmux-app::layout` resolver), and the daemon's smallest-wins size negotiation
/// reconciles the differing per-client cell sizes. `ratios.len() == children.len()`.
///
/// Permille (not `f32`) is deliberate: it keeps the tree deterministic and safe
/// to compare for change-suppression. Never compare layouts with float equality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutNode {
    Leaf {
        pane_index: u32,
    },
    Split {
        dir: SplitDir,
        /// Child weights in permille (0..=1000); same length as `children`.
        ratios: Vec<u16>,
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    /// A single-pane leaf layout (the default for a freshly created tab).
    pub fn single(pane_index: u32) -> Self {
        LayoutNode::Leaf { pane_index }
    }

    /// Collect the `pane_index` of every leaf, left-to-right depth-first.
    pub fn leaves(&self) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<u32>) {
        match self {
            LayoutNode::Leaf { pane_index } => out.push(*pane_index),
            LayoutNode::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }
}

/// One tab: a named tiling layout over a subset of the session's panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub tab_index: TabIndex,
    /// Human-readable display name (default: the 1-based tab number).
    pub name: String,
    /// The tab's tiling layout tree (leaves reference `pane_index`).
    pub layout: LayoutNode,
    /// `pane_index` of the leaf that currently has input focus within this tab.
    pub focused_pane: u32,
}

/// Full session listing entry returned by `SessionList` and related messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    /// Flat list of every pane (PTY) in the session, regardless of tab. Chrome
    /// (titles, status, attach state) reads this; the per-tab `layout` trees
    /// reference these panes by `pane_index`.
    pub panes: Vec<PaneInfo>,
    /// The session's tabs (tiling layouts). Always at least one.
    pub tabs: Vec<TabInfo>,
    /// The tab index the server restored/created as the default view. Which tab
    /// a *client* is actually viewing is client-local state.
    pub active_tab: TabIndex,
    /// The federated peer this session is being viewed through, or `None` for a
    /// local session. Set by the hub's `localize_entry` when it proxies a remote
    /// peer's session list, so clients can group and attribute sessions by
    /// machine without parsing the decorated display name. This is a per-listing,
    /// hub-assigned attribute (not part of the immutable [`SessionMeta`], and not
    /// persisted). `#[serde(default)]` keeps it optional in source; the
    /// exact-match `PROTOCOL_VERSION` handshake guarantees both ends agree on the
    /// wire shape.
    #[serde(default)]
    pub peer: Option<PeerId>,
}

/// Input control mode for a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    /// Any authenticated client may send input.
    Open,
    /// Only the identified client may send input.
    Locked(ClientId),
    /// No client may send input (read-only).
    Disabled,
}

/// Lifecycle event relayed from the server's event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionEventMsg {
    /// A new session (with its initial pane) was created.
    SessionCreated { word_id: WordId },
    /// A session and all its panes were closed.
    SessionClosed { word_id: WordId },
    /// A session was renamed.
    SessionRenamed { word_id: WordId, new_name: String },

    /// A new pane was spawned inside a session.
    PaneSpawned { pane_id: PaneId },
    /// A pane's child process exited.
    PaneExited {
        pane_id: PaneId,
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// A pane was resized.
    PaneResized { pane_id: PaneId, size: TermSize },
    /// A pane's program reported a new window title (OSC 0/2).
    PaneTitleChanged { pane_id: PaneId, title: String },
    /// A pane's program wrote the clipboard via OSC 52. `selection` is the
    /// normalized target ("c"/"p"/"s"/"0".."7"); `data` is the still
    /// base64-encoded payload (decoded client-side at the clipboard leaf).
    PaneClipboardCopy {
        pane_id: PaneId,
        selection: String,
        data: String,
    },
    /// A pane was closed.
    PaneClosed { pane_id: PaneId },

    /// A new tab was created inside a session.
    TabCreated {
        word_id: WordId,
        tab_index: TabIndex,
    },
    /// A tab (and any panes unique to it) was closed.
    TabClosed {
        word_id: WordId,
        tab_index: TabIndex,
    },
    /// A tab's display name changed.
    TabRenamed {
        word_id: WordId,
        tab_index: TabIndex,
        name: String,
    },
    /// A tab's layout tree and/or focus changed. Clients should reconcile to the
    /// authoritative tree carried by the next `LayoutUpdate` for this tab.
    LayoutChanged {
        word_id: WordId,
        tab_index: TabIndex,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_size_pixel_fields_roundtrip() {
        let size = TermSize {
            rows: 40,
            cols: 120,
            pixel_width: 1920,
            pixel_height: 1080,
        };
        let bytes = postcard::to_allocvec(&size).expect("serialize");
        let decoded: TermSize = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.rows, 40);
        assert_eq!(decoded.cols, 120);
        assert_eq!(decoded.pixel_width, 1920);
        assert_eq!(decoded.pixel_height, 1080);
    }

    #[test]
    fn term_size_default_has_zero_pixel_dims() {
        let d = TermSize::default();
        assert_eq!(d.rows, 24);
        assert_eq!(d.cols, 80);
        assert_eq!(d.pixel_width, 0);
        assert_eq!(d.pixel_height, 0);
    }

    #[test]
    fn peer_target_peer_id_and_roundtrip() {
        // SSH: user@host:port when a port override is set.
        let ssh = PeerTarget::Ssh {
            user: Some("alice".into()),
            host: "box".into(),
            ssh_port: Some(2222),
            accept_invalid_certs: true,
        };
        assert_eq!(ssh.peer_id(), "alice@box:2222");
        assert!(ssh.accept_invalid_certs());
        // No user, default port -> bare host.
        assert_eq!(
            PeerTarget::Ssh {
                user: None,
                host: "srv".into(),
                ssh_port: None,
                accept_invalid_certs: false,
            }
            .peer_id(),
            "srv"
        );
        // User, default port -> user@host (no :port suffix).
        assert_eq!(
            PeerTarget::Ssh {
                user: Some("bob".into()),
                host: "h".into(),
                ssh_port: None,
                accept_invalid_certs: false,
            }
            .peer_id(),
            "bob@h"
        );
        // Direct: host:port.
        let direct = PeerTarget::Direct {
            host: "127.0.0.1".into(),
            port: 8443,
            token: "tok".into(),
            accept_invalid_certs: true,
        };
        assert_eq!(direct.peer_id(), "127.0.0.1:8443");

        for t in [ssh, direct] {
            let bytes = postcard::to_allocvec(&t).expect("serialize");
            let decoded: PeerTarget = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(decoded, t);
        }
    }

    #[test]
    fn pane_title_changed_roundtrips() {
        let msg = SessionEventMsg::PaneTitleChanged {
            pane_id: "eagle/0".to_string(),
            title: "~/dev/kmux".to_string(),
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneTitleChanged { pane_id, title } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(title, "~/dev/kmux");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pane_clipboard_copy_roundtrips() {
        let msg = SessionEventMsg::PaneClipboardCopy {
            pane_id: "eagle/0".to_string(),
            selection: "c".to_string(),
            data: "aGVsbG8=".to_string(),
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneClipboardCopy {
                pane_id,
                selection,
                data,
            } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(selection, "c");
                assert_eq!(data, "aGVsbG8=");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn layout_node_nested_roundtrips() {
        // A 2-level tree: a horizontal split whose right child is a vertical split.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![400, 600],
            children: vec![
                LayoutNode::Leaf { pane_index: 0 },
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    ratios: vec![500, 500],
                    children: vec![
                        LayoutNode::Leaf { pane_index: 1 },
                        LayoutNode::Leaf { pane_index: 2 },
                    ],
                },
            ],
        };
        let bytes = postcard::to_allocvec(&tree).expect("serialize");
        let decoded: LayoutNode = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded, tree);
        assert_eq!(decoded.leaves(), vec![0, 1, 2]);
    }

    #[test]
    fn tab_info_roundtrips() {
        let tab = TabInfo {
            tab_index: 3,
            name: "build".into(),
            layout: LayoutNode::Split {
                dir: SplitDir::Vertical,
                ratios: vec![700, 300],
                children: vec![
                    LayoutNode::Leaf { pane_index: 5 },
                    LayoutNode::Leaf { pane_index: 6 },
                ],
            },
            focused_pane: 6,
        };
        let bytes = postcard::to_allocvec(&tab).expect("serialize");
        let decoded: TabInfo = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.tab_index, 3);
        assert_eq!(decoded.name, "build");
        assert_eq!(decoded.focused_pane, 6);
        assert_eq!(decoded.layout.leaves(), vec![5, 6]);
    }

    #[test]
    fn session_entry_with_tabs_roundtrips() {
        let entry = SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: "eagle".into(),
                name: "kmux".into(),
                cwd: "/dev/kmux".into(),
            },
            panes: vec![],
            tabs: vec![TabInfo {
                tab_index: 0,
                name: "1".into(),
                layout: LayoutNode::single(0),
                focused_pane: 0,
            }],
            active_tab: 0,
            peer: None,
        };
        let bytes = postcard::to_allocvec(&entry).expect("serialize");
        let decoded: SessionEntry = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded.tabs.len(), 1);
        assert_eq!(decoded.active_tab, 0);
        assert_eq!(decoded.tabs[0].layout, LayoutNode::single(0));
    }

    #[test]
    fn tab_lifecycle_events_roundtrip() {
        for msg in [
            SessionEventMsg::TabCreated {
                word_id: "eagle".into(),
                tab_index: 1,
            },
            SessionEventMsg::TabClosed {
                word_id: "eagle".into(),
                tab_index: 1,
            },
            SessionEventMsg::TabRenamed {
                word_id: "eagle".into(),
                tab_index: 1,
                name: "logs".into(),
            },
            SessionEventMsg::LayoutChanged {
                word_id: "eagle".into(),
                tab_index: 0,
            },
        ] {
            let bytes = postcard::to_allocvec(&msg).expect("serialize");
            let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
            // Round-trips to an equal-shaped event (spot check the discriminant).
            assert_eq!(
                std::mem::discriminant(&decoded),
                std::mem::discriminant(&msg)
            );
        }
    }

    #[test]
    fn pane_resized_carries_term_size() {
        let msg = SessionEventMsg::PaneResized {
            pane_id: "eagle/0".to_string(),
            size: TermSize {
                rows: 30,
                cols: 100,
                pixel_width: 1000,
                pixel_height: 600,
            },
        };
        let bytes = postcard::to_allocvec(&msg).expect("serialize");
        let decoded: SessionEventMsg = postcard::from_bytes(&bytes).expect("deserialize");
        match decoded {
            SessionEventMsg::PaneResized { pane_id, size } => {
                assert_eq!(pane_id, "eagle/0");
                assert_eq!(size.rows, 30);
                assert_eq!(size.pixel_width, 1000);
            }
            _ => panic!("wrong variant"),
        }
    }
}

/// Error codes for structured error responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    AuthFailed,
    SessionNotFound,
    SessionAlreadyExists,
    NotAuthenticated,
    InvalidMessage,
    InternalError,
    InputLocked,
    InputDisabled,
    /// The daemon has reached the 1000 active session limit.
    SessionLimitReached,
    /// The specified pane was not found.
    PaneNotFound,
}
