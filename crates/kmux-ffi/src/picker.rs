//! The overlays — command hints, the session/server/directory pickers, and
//! the launcher.

use super::*;

/// One autocomplete row for the `/`-command palette. Mirrors `cmd::hint::Hint`;
/// the internal `replace_from` byte offset is omitted (a native text field
/// re-queries [`KmuxDriver::command_hints`] on each change instead of editing
/// char-by-char).
#[derive(uniffi::Record)]
pub struct FfiCommandHint {
    pub display: String,
    pub summary: String,
    pub replacement: String,
    pub append_space: bool,
}

/// Which picker overlay is open.
#[derive(uniffi::Enum)]
pub enum FfiPickerKind {
    Session,
    Directory,
}

/// One row in a picker list.
#[derive(uniffi::Record)]
pub struct FfiPickerEntry {
    pub label: String,
    pub detail: String,
}

/// The open picker's full state, for generic native rendering. Driven via
/// `set_picker_search` / `set_picker_selected` / `activate_picker` /
/// `submit_directory` / `cancel_picker`.
#[derive(uniffi::Record)]
pub struct FfiPicker {
    pub kind: FfiPickerKind,
    pub query: String,
    pub selected: u32,
    pub entries: Vec<FfiPickerEntry>,
}

/// The role of a directory-browser row, so the native UI can render the right
/// glyph and the activation is unambiguous.
#[derive(uniffi::Enum, PartialEq, Eq)]
pub enum FfiDirRowKind {
    /// Create a new session in the browsed directory (row 0).
    CreateHere,
    /// Navigate up to the parent directory.
    Up,
    /// Navigate into a subdirectory.
    Enter,
}

/// One row in the directory browser (the "new session — choose a directory"
/// overlay).
#[derive(uniffi::Record)]
pub struct FfiDirRow {
    pub kind: FfiDirRowKind,
    /// A user-facing label (the directory name, the parent path, or the
    /// "new session in …" affordance).
    pub label: String,
    /// The target path this row acts on (the browsed dir for `CreateHere`, the
    /// parent for Up, the subdir for Enter).
    pub path: String,
}

/// The directory browser's full state, for native rendering. The list lets the
/// user navigate the daemon host's filesystem (so it works for a remote daemon)
/// and pick where a new session is created. Driven via `set_picker_search`
/// (filter), `set_picker_selected`, and `submit_directory` / `activate_picker`
/// (which create-here or navigate based on the selected row); `cancel_picker`
/// dismisses it.
#[derive(uniffi::Record)]
pub struct FfiDirBrowser {
    /// The directory currently being browsed.
    pub cwd: String,
    /// The current filter text.
    pub query: String,
    /// The highlighted row index.
    pub selected: u32,
    /// The browsable rows in render order (`CreateHere`, optional Up, subdirs).
    pub rows: Vec<FfiDirRow>,
    /// A listing error to surface (e.g. permission denied), if any.
    pub error: Option<String>,
}

/// Connection status of a remote in the launcher (issue #121), mirroring
/// [`RemoteStatus`]. The error reason is carried on the row's `detail`.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiRemoteStatus {
    Idle,
    Connecting,
    Connected,
    Error,
}

/// The role of a launcher row, so the native UI renders the right control and
/// activation is unambiguous.
#[derive(uniffi::Enum, Debug, PartialEq, Eq)]
pub enum FfiLaunchRowKind {
    /// Open a new local session (opens the directory browser).
    LocalNewSession,
    /// Attach an existing local session.
    LocalExisting,
    /// A remote's header/toggle row (expand connects on focus).
    Remote,
    /// Open a new session on the remote (opens the path prompt).
    RemoteNewSession,
    /// Attach an existing session on the remote.
    RemoteExisting,
    /// Restore a closed (inactive) local session from the graveyard (issue #64).
    ClosedSession,
    /// Add a new remote (opens the add-remote form).
    AddRemote,
}

/// One row in the unified session launcher (issue #121), flattened for native
/// rendering. `peer`/`word_id` carry the routing keys; `status`/`expanded` drive
/// a remote header; `active` marks the focused session.
#[derive(uniffi::Record)]
pub struct FfiLaunchRow {
    pub kind: FfiLaunchRowKind,
    pub label: String,
    /// Secondary text: a session's cwd, or a remote's status / error reason.
    pub detail: String,
    pub peer: Option<String>,
    pub word_id: Option<String>,
    pub status: FfiRemoteStatus,
    pub expanded: bool,
    pub active: bool,
}

/// The launcher's full state, for native rendering. Driven via the generic
/// `set_picker_search` / `set_picker_selected` / `activate_picker` /
/// `cancel_picker` (the launcher is a picker), plus `launch_*` helpers.
#[derive(uniffi::Record)]
pub struct FfiLaunchPicker {
    pub query: String,
    pub selected: u32,
    pub rows: Vec<FfiLaunchRow>,
}

/// Values for the add-remote form (issue #121), mirroring [`AddRemoteForm`].
#[derive(uniffi::Record)]
pub struct FfiAddRemoteForm {
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub accept_invalid_certs: bool,
}

impl From<FfiAddRemoteForm> for AddRemoteForm {
    fn from(f: FfiAddRemoteForm) -> Self {
        Self {
            host: f.host,
            user: f.user,
            port: f.port,
            accept_invalid_certs: f.accept_invalid_certs,
        }
    }
}

pub(crate) fn remote_status_to_ffi(s: &RemoteStatus) -> FfiRemoteStatus {
    match s {
        RemoteStatus::Idle => FfiRemoteStatus::Idle,
        RemoteStatus::Connecting => FfiRemoteStatus::Connecting,
        RemoteStatus::Connected => FfiRemoteStatus::Connected,
        RemoteStatus::Error(_) => FfiRemoteStatus::Error,
    }
}

/// Flatten a [`LaunchRow`] into its FFI projection.
pub(crate) fn launch_row_to_ffi(row: LaunchRow) -> FfiLaunchRow {
    let idle = FfiLaunchRow {
        kind: FfiLaunchRowKind::AddRemote,
        label: String::new(),
        detail: String::new(),
        peer: None,
        word_id: None,
        status: FfiRemoteStatus::Idle,
        expanded: false,
        active: false,
    };
    match row {
        LaunchRow::LocalNewSession { default_cwd } => FfiLaunchRow {
            kind: FfiLaunchRowKind::LocalNewSession,
            label: "New local session".to_string(),
            detail: default_cwd,
            ..idle
        },
        LaunchRow::LocalExisting {
            word_id,
            name,
            cwd,
            active,
        } => FfiLaunchRow {
            kind: FfiLaunchRowKind::LocalExisting,
            label: name,
            detail: cwd,
            word_id: Some(word_id),
            active,
            ..idle
        },
        LaunchRow::Remote {
            peer,
            label,
            status,
            expanded,
        } => {
            let detail = match &status {
                RemoteStatus::Error(reason) => reason.clone(),
                RemoteStatus::Connecting => "connecting…".to_string(),
                RemoteStatus::Connected => "connected".to_string(),
                RemoteStatus::Idle => String::new(),
            };
            FfiLaunchRow {
                kind: FfiLaunchRowKind::Remote,
                label,
                detail,
                peer: Some(peer),
                status: remote_status_to_ffi(&status),
                expanded,
                ..idle
            }
        }
        LaunchRow::RemoteNewSession { peer } => FfiLaunchRow {
            kind: FfiLaunchRowKind::RemoteNewSession,
            label: "New session…".to_string(),
            peer: Some(peer),
            status: FfiRemoteStatus::Connected,
            ..idle
        },
        LaunchRow::RemoteExisting {
            peer,
            word_id,
            name,
            cwd,
            active,
        } => FfiLaunchRow {
            kind: FfiLaunchRowKind::RemoteExisting,
            label: name,
            detail: cwd,
            peer: Some(peer),
            word_id: Some(word_id),
            status: FfiRemoteStatus::Connected,
            active,
            ..idle
        },
        LaunchRow::ClosedSession {
            word_id,
            name,
            cwd,
            last_active_ms,
        } => {
            let when = kmux_app::core::relative_time_label(last_active_ms);
            let detail = if cwd.is_empty() {
                when
            } else {
                format!("{cwd} · {when}")
            };
            FfiLaunchRow {
                kind: FfiLaunchRowKind::ClosedSession,
                label: name,
                detail,
                word_id: Some(word_id),
                ..idle
            }
        }
        LaunchRow::AddRemote => FfiLaunchRow {
            kind: FfiLaunchRowKind::AddRemote,
            label: "Add remote…".to_string(),
            ..idle
        },
    }
}

/// A user-facing label for a directory-browser row, shared by the generic
/// `picker()` getter and the structured `dir_browser()` getter so both render
/// the row identically.
pub(crate) fn dir_row_label(row: &DirBrowserRow) -> String {
    match row {
        DirBrowserRow::CreateHere { cwd } => format!("＋  New session in {cwd}"),
        DirBrowserRow::Up { parent } => format!("..  {parent}"),
        DirBrowserRow::Enter { name, .. } => format!("📁  {name}"),
    }
}
