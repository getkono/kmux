use std::time::Instant;

use kmux_client::pipeline::ResolvedTarget;
use kmux_client::session_manager::SessionManager;
use kmux_client::ssh::RemoteTarget;

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
    pub metrics_overlay_visible: bool,
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

    /// SSH target stored for re-negotiation when the tunnel dies (SSH mode only).
    pub(super) ssh_target: Option<RemoteTarget>,

    /// Target for the initial bootstrap. Consumed on the first connect.
    pub(super) pending_target: Option<ResolvedTarget>,
}

impl App {
    pub fn new(
        target: ResolvedTarget,
        initial_cwd: String,
        theme: Theme,
        instance_id: String,
        auto_session: Option<String>,
        auto_cwd: Option<String>,
    ) -> Self {
        use crate::mode::{ConnectField, Mode};

        let (is_local, connect_host, connect_port, connect_token, accept_invalid_certs, ssh_target) =
            match &target {
                ResolvedTarget::LocalDaemon => (
                    true,
                    "127.0.0.1".to_string(),
                    String::new(),
                    String::new(),
                    true,
                    None,
                ),
                ResolvedTarget::Direct {
                    host,
                    port,
                    token,
                    accept_invalid_certs,
                } => (
                    false,
                    host.clone(),
                    port.to_string(),
                    token.clone(),
                    *accept_invalid_certs,
                    None,
                ),
                ResolvedTarget::Ssh {
                    target,
                    accept_invalid_certs,
                } => (
                    false,
                    String::new(),
                    String::new(),
                    String::new(),
                    *accept_invalid_certs,
                    Some(target.clone()),
                ),
            };

        // Direct-with-no-token is the only case that needs the Connect form;
        // every other path (Local, SSH, Direct with token) is ready to bootstrap.
        let initial_mode = match &target {
            ResolvedTarget::Direct { token, .. } if token.is_empty() => Mode::Connect {
                field: ConnectField::Host,
            },
            _ => Mode::Normal,
        };

        let capabilities = crate::host_caps::detect();

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
            let port_num: u16 = connect_port.parse().unwrap_or(8443);
            let s = format!("{}:{}", connect_host, port_num);
            (
                s.clone(),
                s,
                ServerKind::Direct {
                    host: connect_host.clone(),
                    port: port_num,
                },
            )
        };

        // Seed SessionManager with placeholder host/port/token. `apply_outcome`
        // overwrites these when the bootstrap completes.
        let port_num: u16 = connect_port.parse().unwrap_or(0);
        let mut mgr = SessionManager::new(
            connect_host.clone(),
            port_num,
            connect_token.clone(),
            accept_invalid_certs,
            capabilities,
        );
        mgr.enable_metrics_persistence();

        Self {
            mgr,
            theme,
            mode: initial_mode,
            hud_visible: false,
            metrics_overlay_visible: false,
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
            ssh_target,
            pending_target: Some(target),
            auto_session,
            auto_cwd,
        }
    }
}
