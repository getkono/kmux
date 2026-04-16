use std::time::Instant;

use kmux_client::session_manager::SessionManager;
use kmux_client::ssh::{RemoteTarget, SshSession};

use crate::recent_servers::{RecentServersCache, ServerKind};
use crate::theme::Theme;

mod event_loop;
mod helpers;
mod key_handler;
mod mouse_handler;

/// What `handle_key` returns to the event loop.
pub(super) enum KeyResult {
    Continue,
    Quit,
    /// User submitted the Connect form; the event loop must replace `srv_rx`.
    Reconnect,
    /// User selected a server from the server picker.
    SwitchServer(SwitchTarget),
}

/// Destination chosen from the server picker.
pub(super) enum SwitchTarget {
    Local,
    Ssh(kmux_client::ssh::RemoteTarget),
    Direct { host: String, port: u16 },
}

pub struct App {
    pub mgr: SessionManager,

    // TUI-specific state
    pub theme: Theme,
    pub mode: crate::mode::Mode,
    pub hud_visible: bool,
    pub force_snapshot_mode: bool,

    // Connect form input fields
    pub connect_host: String,
    pub connect_port: String,
    pub connect_token: String,

    // Reconnection bookkeeping
    pub disconnect_at: Option<Instant>,

    // Session picker state
    pub session_picker_selected: usize,
    pub session_picker_search: String,

    // Directory picker state (remote connections)
    pub dir_picker_buffer: String,
    pub dir_picker_selected: usize,

    // Auto-session selection context
    pub is_local: bool,
    pub initial_cwd: String,
    pub(super) did_auto_select: bool,

    /// CLI `--session` flag: auto-attach to a session by name or word_id.
    pub(super) auto_session: Option<String>,
    /// Effective cwd from `--cwd` or `:path` in server string.
    pub(super) auto_cwd: Option<String>,

    /// Width (in columns) of the server badge in the top bar.
    pub server_badge_cols: u16,

    /// Width (in columns) of the session badge in the top bar, used to detect
    /// mouse clicks that should open the session picker.
    pub session_badge_cols: u16,

    /// Human-readable label for the current server shown in the server badge.
    pub server_display: String,

    /// Cache key for the current server (empty for local).
    pub(super) server_string: String,

    /// Connection kind for the current server (used for reconnect routing).
    pub(super) server_kind: ServerKind,

    // Server picker state
    pub server_picker_selected: usize,
    pub server_picker_search: String,

    /// Persisted recent-servers cache.
    pub recent_servers: RecentServersCache,

    pub(super) needs_render: bool,

    /// Unique ID for this client process, written to the connection log on auth success.
    pub(super) instance_id: String,

    /// Active SSH session (tunnel process + connection metadata) when in SSH mode.
    /// Kept alive as long as the TCP transport is in use; dropped on QUIC upgrade.
    pub(super) ssh_session: Option<SshSession>,

    /// SSH target stored for re-negotiation when the tunnel dies (SSH mode only).
    pub(super) ssh_target: Option<RemoteTarget>,

    /// Consecutive reconnect failures. Reset on successful auth. Used for exponential backoff.
    pub(super) reconnect_attempt: u32,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port: u16,
        token: String,
        accept_invalid_certs: bool,
        is_local: bool,
        initial_cwd: String,
        theme: Theme,
        instance_id: String,
        ssh_session: Option<SshSession>,
        ssh_target: Option<RemoteTarget>,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
    ) -> Self {
        use crate::mode::{ConnectField, Mode};

        let connect_host = host.clone();
        let connect_port = port.to_string();
        let connect_token = token.clone();

        let initial_mode = if token.is_empty() {
            Mode::Connect {
                field: ConnectField::Host,
            }
        } else {
            Mode::Normal
        };

        let capabilities = crate::host_caps::detect();

        // Compute server display label and cache key from connection parameters.
        let (server_display, server_string, server_kind) = if is_local {
            ("localhost".to_string(), String::new(), ServerKind::Local)
        } else if let Some(ref t) = ssh_target {
            let display = match &t.user {
                Some(u) => format!("{}@{}", u, t.host),
                None => t.host.clone(),
            };
            let kind = ServerKind::Ssh {
                user: t.user.clone(),
                host: t.host.clone(),
                ssh_port: t.ssh_port,
            };
            (display.clone(), display, kind)
        } else {
            let s = format!("{}:{}", host, port);
            (
                s.clone(),
                s,
                ServerKind::Direct {
                    host: host.clone(),
                    port,
                },
            )
        };

        Self {
            mgr: SessionManager::new(host, port, token, accept_invalid_certs, capabilities),
            theme,
            mode: initial_mode,
            hud_visible: false,
            force_snapshot_mode: false,
            connect_host,
            connect_port,
            connect_token,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            is_local,
            initial_cwd,
            did_auto_select: false,
            server_badge_cols: 0,
            session_badge_cols: 0,
            server_display,
            server_string,
            server_kind,
            server_picker_selected: 0,
            server_picker_search: String::new(),
            recent_servers: RecentServersCache::load(),
            needs_render: true,
            instance_id,
            ssh_session,
            ssh_target,
            auto_session,
            auto_cwd,
            reconnect_attempt: 0,
        }
    }
}
