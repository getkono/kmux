//! The static list of built-in commands.
//!
//! Adding a command:
//! 1. Write a `cmd_xxx` function below with signature `fn(&mut App, &[String]) -> CommandResult`.
//! 2. Append a [`CommandSpec`] entry to [`ALL`].
//! 3. (Optional) Add a hint test in `cmd::hint::tests` if it has a non-trivial completer.

use crate::app::App;
use crate::mode::Mode;
use crate::theme;

use super::spec::{ArgSpec, CommandResult, CommandSpec, CommandSuccess, Completer};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn require_arg<'a>(args: &'a [String], idx: usize, name: &str) -> Result<&'a str, String> {
    args.get(idx)
        .map(|s| s.as_str())
        .ok_or_else(|| format!("missing argument <{name}>"))
}

/// Most session/pane operations are no-ops when the daemon is unreachable.
/// Convert that into an explicit error so the user sees a status message
/// instead of silent inaction.
fn require_connected(app: &App) -> Result<(), String> {
    if app.mgr.is_connected() {
        Ok(())
    } else {
        Err("not connected to a daemon".into())
    }
}

fn parse_on_off(s: &str, name: &str) -> Result<bool, String> {
    match s.to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err(format!("expected on|off for <{name}>, got '{s}'")),
    }
}

fn signal_from_name(s: &str) -> Result<i32, String> {
    match s.to_ascii_lowercase().as_str() {
        "kill" | "sigkill" | "9" => Ok(9),
        "term" | "sigterm" | "15" => Ok(15),
        "stop" | "sigstop" | "19" => Ok(19),
        "cont" | "sigcont" | "18" => Ok(18),
        _ => Err(format!("unknown signal '{s}' (kill|term|stop|cont)")),
    }
}

fn current_term_size() -> kmux_protocol::messages::TermSize {
    App::current_term_size()
}

// ── Client / TUI controls ────────────────────────────────────────────────────

fn cmd_quit(_app: &mut App, _args: &[String]) -> CommandResult {
    Ok(CommandSuccess::Quit)
}

fn cmd_redraw(app: &mut App, _args: &[String]) -> CommandResult {
    app.force_clear = true;
    Ok(CommandSuccess::Status("redraw queued".into()))
}

fn cmd_help(app: &mut App, _args: &[String]) -> CommandResult {
    app.mode = Mode::Help;
    Ok(CommandSuccess::Ok)
}

fn cmd_hud(app: &mut App, _args: &[String]) -> CommandResult {
    app.hud_visible = !app.hud_visible;
    Ok(CommandSuccess::Status(format!(
        "hud: {}",
        if app.hud_visible { "on" } else { "off" }
    )))
}

fn cmd_metrics(app: &mut App, _args: &[String]) -> CommandResult {
    app.metrics_overlay_visible = !app.metrics_overlay_visible;
    Ok(CommandSuccess::Status(format!(
        "metrics: {}",
        if app.metrics_overlay_visible {
            "on"
        } else {
            "off"
        }
    )))
}

fn cmd_lock(app: &mut App, _args: &[String]) -> CommandResult {
    if app.mgr.active_pane_id().is_none() {
        return Err("no active pane".into());
    }
    app.mgr.toggle_input_lock();
    Ok(CommandSuccess::Status(format!(
        "input lock: {}",
        if app.mgr.active_input_locked() {
            "on"
        } else {
            "off"
        }
    )))
}

fn cmd_snapshot(app: &mut App, args: &[String]) -> CommandResult {
    let arg = require_arg(args, 0, "on|off")?;
    let val = parse_on_off(arg, "on|off")?;
    app.force_snapshot_mode = val;
    // set_snapshot_mode sends a wire message — only relevant when connected,
    // but the flag is still useful locally so we don't gate it.
    app.mgr.set_snapshot_mode(val);
    Ok(CommandSuccess::Status(format!(
        "snapshot: {}",
        if val { "on" } else { "off" }
    )))
}

fn cmd_theme(app: &mut App, args: &[String]) -> CommandResult {
    let name = require_arg(args, 0, "name")?;
    match theme::builtin_theme(name) {
        Some(t) => {
            app.theme = t;
            Ok(CommandSuccess::Status(format!("theme: {name}")))
        }
        None => Err(format!("unknown theme '{name}'")),
    }
}

fn cmd_clear_history(app: &mut App, _args: &[String]) -> CommandResult {
    let n = app.command_history.len();
    app.command_history.clear();
    Ok(CommandSuccess::Status(format!(
        "cleared {n} history entries"
    )))
}

// ── Connection / server ──────────────────────────────────────────────────────

fn cmd_disconnect(app: &mut App, _args: &[String]) -> CommandResult {
    app.mgr.disconnect();
    app.mode = Mode::Normal;
    Ok(CommandSuccess::Ok)
}

fn cmd_reconnect(_app: &mut App, _args: &[String]) -> CommandResult {
    Ok(CommandSuccess::Reconnect)
}

fn cmd_server(app: &mut App, _args: &[String]) -> CommandResult {
    app.mode = Mode::ServerPicker;
    app.server_picker_search.clear();
    app.server_picker_selected = 0;
    Ok(CommandSuccess::Ok)
}

fn cmd_local(_app: &mut App, _args: &[String]) -> CommandResult {
    Ok(CommandSuccess::SwitchServer(
        crate::app::SwitchTarget::Local,
    ))
}

// ── Sessions ─────────────────────────────────────────────────────────────────

fn cmd_session_new(app: &mut App, args: &[String]) -> CommandResult {
    require_connected(app)?;
    let name = args.first().map(String::as_str);
    let cwd = args.get(1).map(String::as_str);
    app.mgr.create_session(name, cwd, current_term_size());
    Ok(CommandSuccess::Status(match name {
        Some(n) => format!("creating session '{n}'…"),
        None => "creating session…".into(),
    }))
}

fn cmd_session_close(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    let Some(word_id) = app.mgr.active_session().map(|s| s.to_string()) else {
        return Err("no active session".into());
    };
    app.mode = Mode::ConfirmCloseSession { word_id };
    Ok(CommandSuccess::Ok)
}

fn cmd_session_rename(app: &mut App, args: &[String]) -> CommandResult {
    require_connected(app)?;
    let new_name = require_arg(args, 0, "name")?.trim();
    if new_name.is_empty() {
        return Err("name cannot be empty".into());
    }
    let Some(word_id) = app.mgr.active_session().map(|s| s.to_string()) else {
        return Err("no active session".into());
    };
    app.mgr.rename_session(&word_id, new_name);
    Ok(CommandSuccess::Status(format!("renamed to '{new_name}'")))
}

fn cmd_session_next(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.session_list().is_empty() {
        return Err("no sessions".into());
    }
    app.mgr.cycle_session(1);
    Ok(CommandSuccess::Ok)
}

fn cmd_session_prev(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.session_list().is_empty() {
        return Err("no sessions".into());
    }
    app.mgr.cycle_session(-1);
    Ok(CommandSuccess::Ok)
}

fn cmd_session_switch(app: &mut App, args: &[String]) -> CommandResult {
    require_connected(app)?;
    let query = require_arg(args, 0, "query")?;
    if app.mgr.session_list().is_empty() {
        return Err("no sessions".into());
    }
    // Allow numeric index (1-based, like the existing 0-9 keybind).
    if let Ok(idx) = query.parse::<usize>() {
        if idx == 0 || idx > app.mgr.session_list().len() {
            return Err(format!("session index {idx} out of range"));
        }
        let word_id = app.mgr.session_list()[idx - 1].meta.word_id.clone();
        app.mgr.select_session(word_id);
        return Ok(CommandSuccess::Status(format!("switched to #{idx}")));
    }
    if let Some(word_id) = app.mgr.find_session_by_name(query) {
        app.mgr.select_session(word_id);
        return Ok(CommandSuccess::Status(format!("switched to '{query}'")));
    }
    if app
        .mgr
        .session_list()
        .iter()
        .any(|e| e.meta.word_id == query)
    {
        app.mgr.select_session(query.to_string());
        return Ok(CommandSuccess::Status(format!("switched to '{query}'")));
    }
    Err(format!("no session matches '{query}'"))
}

fn cmd_session_list(app: &mut App, _args: &[String]) -> CommandResult {
    app.mode = Mode::SessionPicker;
    app.session_picker_search.clear();
    app.session_picker_selected = 0;
    Ok(CommandSuccess::Ok)
}

// ── Panes ────────────────────────────────────────────────────────────────────

fn cmd_pane_new(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.active_session().is_none() {
        return Err("no active session".into());
    }
    app.mgr.create_pane(current_term_size());
    Ok(CommandSuccess::Status("creating pane…".into()))
}

fn cmd_pane_close(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.active_pane_id().is_none() {
        return Err("no active pane".into());
    }
    app.mgr.close_pane();
    Ok(CommandSuccess::Status("closing pane…".into()))
}

fn cmd_pane_next(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.active_pane_id().is_none() {
        return Err("no active pane".into());
    }
    app.mgr.cycle_pane(1);
    Ok(CommandSuccess::Ok)
}

fn cmd_pane_prev(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    if app.mgr.active_pane_id().is_none() {
        return Err("no active pane".into());
    }
    app.mgr.cycle_pane(-1);
    Ok(CommandSuccess::Ok)
}

fn cmd_signal(app: &mut App, args: &[String]) -> CommandResult {
    require_connected(app)?;
    let name = require_arg(args, 0, "signal")?;
    let signum = signal_from_name(name)?;
    let Some(pane_id) = app.mgr.active_pane_id().map(|s| s.to_string()) else {
        return Err("no active pane".into());
    };
    app.mgr.send_signal(&pane_id, signum);
    Ok(CommandSuccess::Status(format!(
        "sent {} to active pane",
        name.to_ascii_uppercase()
    )))
}

// ── Daemon ───────────────────────────────────────────────────────────────────

fn cmd_daemon_status(app: &mut App, _args: &[String]) -> CommandResult {
    let server = if app.server_display.is_empty() {
        "(local)"
    } else {
        app.server_display.as_str()
    };
    let version = app.mgr.server_version.as_deref().unwrap_or("unknown");
    let sessions = app.mgr.session_list().len();
    let conn = if app.mgr.is_connected() {
        "connected"
    } else {
        "disconnected"
    };
    Ok(CommandSuccess::Status(format!(
        "{server} · v{version} · {sessions} sessions · {conn}"
    )))
}

fn cmd_daemon_ping(app: &mut App, _args: &[String]) -> CommandResult {
    require_connected(app)?;
    // SessionManager keeps wire-level Ping/Pong internal to the liveness layer;
    // a `request_session_list` is the most-similar user-triggerable round-trip
    // and gives visible feedback (refreshed tab bar) when the daemon answers.
    app.mgr.request_session_list();
    Ok(CommandSuccess::Status(
        "ping → daemon (refresh sent)".into(),
    ))
}

// ── Registry ─────────────────────────────────────────────────────────────────

const NO_ARGS: &[ArgSpec] = &[];

const ARGS_NAME_OPT: &[ArgSpec] = &[
    ArgSpec {
        name: "name",
        required: false,
        completer: Completer::None,
    },
    ArgSpec {
        name: "cwd",
        required: false,
        completer: Completer::None,
    },
];

const ARGS_NAME_REQ: &[ArgSpec] = &[ArgSpec {
    name: "name",
    required: true,
    completer: Completer::None,
}];

const ARGS_QUERY_REQ: &[ArgSpec] = &[ArgSpec {
    name: "name|id|index",
    required: true,
    completer: Completer::Sessions,
}];

const ARGS_THEME_REQ: &[ArgSpec] = &[ArgSpec {
    name: "name",
    required: true,
    completer: Completer::Themes,
}];

const ARGS_SIGNAL_REQ: &[ArgSpec] = &[ArgSpec {
    name: "signal",
    required: true,
    completer: Completer::Signals,
}];

const ARGS_ON_OFF: &[ArgSpec] = &[ArgSpec {
    name: "on|off",
    required: true,
    completer: Completer::OnOff,
}];

/// All built-in commands. Order influences hint ordering on ties.
pub static ALL: &[CommandSpec] = &[
    CommandSpec {
        name: "quit",
        aliases: &["q", "exit"],
        summary: "Quit the TUI",
        args: NO_ARGS,
        run: cmd_quit,
    },
    CommandSpec {
        name: "redraw",
        aliases: &[],
        summary: "Force a full redraw",
        args: NO_ARGS,
        run: cmd_redraw,
    },
    CommandSpec {
        name: "help",
        aliases: &["?"],
        summary: "Open the help overlay",
        args: NO_ARGS,
        run: cmd_help,
    },
    CommandSpec {
        name: "hud",
        aliases: &[],
        summary: "Toggle the HUD",
        args: NO_ARGS,
        run: cmd_hud,
    },
    CommandSpec {
        name: "metrics",
        aliases: &[],
        summary: "Toggle the metrics overlay",
        args: NO_ARGS,
        run: cmd_metrics,
    },
    CommandSpec {
        name: "lock",
        aliases: &["unlock"],
        summary: "Toggle input lock for the active pane",
        args: NO_ARGS,
        run: cmd_lock,
    },
    CommandSpec {
        name: "snapshot",
        aliases: &[],
        summary: "Force snapshot rendering on/off",
        args: ARGS_ON_OFF,
        run: cmd_snapshot,
    },
    CommandSpec {
        name: "theme",
        aliases: &[],
        summary: "Switch the colour theme",
        args: ARGS_THEME_REQ,
        run: cmd_theme,
    },
    CommandSpec {
        name: "clear-history",
        aliases: &[],
        summary: "Wipe the command-palette history",
        args: NO_ARGS,
        run: cmd_clear_history,
    },
    CommandSpec {
        name: "disconnect",
        aliases: &[],
        summary: "Drop the connection and show the Connect form",
        args: NO_ARGS,
        run: cmd_disconnect,
    },
    CommandSpec {
        name: "reconnect",
        aliases: &[],
        summary: "Force a reconnect",
        args: NO_ARGS,
        run: cmd_reconnect,
    },
    CommandSpec {
        name: "server",
        aliases: &[],
        summary: "Open the server picker",
        args: NO_ARGS,
        run: cmd_server,
    },
    CommandSpec {
        name: "local",
        aliases: &[],
        summary: "Switch to the local UDS daemon",
        args: NO_ARGS,
        run: cmd_local,
    },
    CommandSpec {
        name: "session new",
        aliases: &["s new"],
        summary: "Create a new session",
        args: ARGS_NAME_OPT,
        run: cmd_session_new,
    },
    CommandSpec {
        name: "session close",
        aliases: &["s close"],
        summary: "Close the active session (with confirmation)",
        args: NO_ARGS,
        run: cmd_session_close,
    },
    CommandSpec {
        name: "session rename",
        aliases: &["s rename"],
        summary: "Rename the active session",
        args: ARGS_NAME_REQ,
        run: cmd_session_rename,
    },
    CommandSpec {
        name: "session next",
        aliases: &["s next"],
        summary: "Switch to the next session",
        args: NO_ARGS,
        run: cmd_session_next,
    },
    CommandSpec {
        name: "session prev",
        aliases: &["s prev"],
        summary: "Switch to the previous session",
        args: NO_ARGS,
        run: cmd_session_prev,
    },
    CommandSpec {
        name: "session switch",
        aliases: &["s switch"],
        summary: "Switch to a session by name, id, or 1-based index",
        args: ARGS_QUERY_REQ,
        run: cmd_session_switch,
    },
    CommandSpec {
        name: "session list",
        aliases: &["s list"],
        summary: "Open the session picker",
        args: NO_ARGS,
        run: cmd_session_list,
    },
    CommandSpec {
        name: "pane new",
        aliases: &["p new"],
        summary: "Create a new pane in the active session",
        args: NO_ARGS,
        run: cmd_pane_new,
    },
    CommandSpec {
        name: "pane close",
        aliases: &["p close"],
        summary: "Close the active pane",
        args: NO_ARGS,
        run: cmd_pane_close,
    },
    CommandSpec {
        name: "pane next",
        aliases: &["p next"],
        summary: "Switch to the next pane",
        args: NO_ARGS,
        run: cmd_pane_next,
    },
    CommandSpec {
        name: "pane prev",
        aliases: &["p prev"],
        summary: "Switch to the previous pane",
        args: NO_ARGS,
        run: cmd_pane_prev,
    },
    CommandSpec {
        name: "signal",
        aliases: &[],
        summary: "Send a Unix signal to the active pane",
        args: ARGS_SIGNAL_REQ,
        run: cmd_signal,
    },
    CommandSpec {
        name: "daemon status",
        aliases: &["d status"],
        summary: "Show the connected daemon's identity and version",
        args: NO_ARGS,
        run: cmd_daemon_status,
    },
    CommandSpec {
        name: "daemon ping",
        aliases: &["d ping"],
        summary: "Force a session-list refresh round-trip",
        args: NO_ARGS,
        run: cmd_daemon_ping,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::exec::{Outcome, run};
    use crate::mode::CommandState;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::ClientCapabilities;

    fn fixture_app() -> App {
        let mut mgr = SessionManager::new(
            "127.0.0.1".into(),
            8443,
            "tok".into(),
            true,
            ClientCapabilities::default(),
        );
        // Pretend the daemon is connected so commands that gate on
        // `is_connected` reach their real error paths in tests.
        mgr.connected = true;
        App {
            mgr,
            theme: theme::default_theme(),
            mode: Mode::Command(CommandState::default()),
            hud_visible: false,
            metrics_overlay_visible: false,
            force_snapshot_mode: false,
            disconnect_at: None,
            session_picker_selected: 0,
            session_picker_search: String::new(),
            dir_picker_buffer: String::new(),
            dir_picker_selected: 0,
            is_local: true,
            initial_cwd: String::new(),
            did_auto_select: false,
            auto_session: None,
            auto_cwd: None,
            top_bar_hits: crate::app::TopBarHits::default(),
            picker_hits: crate::app::PickerHits::default(),
            server_display: String::new(),
            server_string: String::new(),
            server_kind: crate::recent_servers::ServerKind::Local,
            server_picker_selected: 0,
            server_picker_search: String::new(),
            recent_servers: crate::recent_servers::RecentServersCache::load(),
            needs_render: true,
            force_clear: false,
            paste_tx: None,
            cancel_tx: None,
            pending_srv_tx: None,
            instance_id: String::new(),
            ssh_target: None,
            pending_target: None,
            last_exit_error: None,
            command_history: std::collections::VecDeque::new(),
        }
    }

    /// Drives the full submit pipeline as `dispatch_action` would: starts in
    /// `Mode::Command(buffer)`, fires `Action::CommandSubmit`, returns the
    /// `KeyResult` that the event loop would observe.
    async fn submit(buffer: &str, app: &mut App) -> crate::app::KeyResult {
        app.mode = Mode::Command(CommandState {
            buffer: buffer.to_string(),
            cursor: buffer.len(),
            selected: 0,
            history_pos: None,
        });
        app.dispatch_action(crate::mode::Action::CommandSubmit, None)
            .await
    }

    #[tokio::test]
    async fn submit_quit_returns_keyresult_quit() {
        let mut app = fixture_app();
        let kr = submit("quit", &mut app).await;
        assert!(matches!(kr, crate::app::KeyResult::Quit));
    }

    #[tokio::test]
    async fn submit_redraw_via_dispatch_sets_force_clear() {
        let mut app = fixture_app();
        let kr = submit("redraw", &mut app).await;
        assert!(matches!(kr, crate::app::KeyResult::Continue));
        assert!(app.force_clear);
    }

    #[tokio::test]
    async fn submit_help_via_dispatch_changes_mode() {
        let mut app = fixture_app();
        let _ = submit("help", &mut app).await;
        assert!(matches!(app.mode, Mode::Help));
    }

    #[tokio::test]
    async fn submit_resets_mode_back_to_normal_on_status_only() {
        let mut app = fixture_app();
        let _ = submit("hud", &mut app).await;
        // hud doesn't change mode; mem::replace should have already set Normal.
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.hud_visible);
    }

    #[tokio::test]
    async fn submit_empty_buffer_is_noop() {
        let mut app = fixture_app();
        let kr = submit("", &mut app).await;
        assert!(matches!(kr, crate::app::KeyResult::Continue));
        assert_eq!(app.command_history.len(), 0);
    }

    #[tokio::test]
    async fn submit_partial_command_falls_back_to_selected_hint() {
        // User types `qu` (a partial command name) and hits Enter without
        // pressing Tab first. The dropdown's top hint resolves to /quit, so
        // Enter should run /quit instead of failing with "unknown command".
        let mut app = fixture_app();
        let kr = submit("qu", &mut app).await;
        assert!(
            matches!(kr, crate::app::KeyResult::Quit),
            "expected fallback to /quit; status: {:?}",
            app.mgr.status_msg()
        );
    }

    #[tokio::test]
    async fn submit_records_history() {
        let mut app = fixture_app();
        let _ = submit("hud", &mut app).await;
        assert_eq!(app.command_history.back().map(|s| s.as_str()), Some("hud"));
    }

    #[test]
    fn quit_returns_quit_outcome() {
        let mut app = fixture_app();
        assert!(matches!(run(&mut app, "quit"), Outcome::Quit));
    }

    #[test]
    fn quit_alias_q_returns_quit() {
        let mut app = fixture_app();
        assert!(matches!(run(&mut app, "q"), Outcome::Quit));
    }

    #[test]
    fn redraw_sets_force_clear() {
        let mut app = fixture_app();
        assert!(!app.force_clear);
        let _ = run(&mut app, "redraw");
        assert!(app.force_clear, "force_clear should be set");
    }

    #[test]
    fn help_changes_mode_to_help() {
        let mut app = fixture_app();
        let _ = run(&mut app, "help");
        assert!(matches!(app.mode, Mode::Help));
    }

    #[test]
    fn hud_toggles() {
        let mut app = fixture_app();
        assert!(!app.hud_visible);
        let _ = run(&mut app, "hud");
        assert!(app.hud_visible);
        let _ = run(&mut app, "hud");
        assert!(!app.hud_visible);
    }

    #[test]
    fn metrics_toggles() {
        let mut app = fixture_app();
        assert!(!app.metrics_overlay_visible);
        let _ = run(&mut app, "metrics");
        assert!(app.metrics_overlay_visible);
    }

    #[test]
    fn theme_changes_palette() {
        let mut app = fixture_app();
        let original_bg = app.theme.bg;
        let _ = run(&mut app, "theme dracula");
        assert_ne!(app.theme.bg, original_bg, "theme should change");
    }

    #[test]
    fn theme_unknown_sets_status() {
        let mut app = fixture_app();
        let _ = run(&mut app, "theme nonsense");
        assert!(
            app.mgr.status_msg().contains("unknown theme"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn snapshot_on_sets_force_snapshot() {
        let mut app = fixture_app();
        let _ = run(&mut app, "snapshot on");
        assert!(app.force_snapshot_mode);
    }

    #[test]
    fn server_opens_picker_mode() {
        let mut app = fixture_app();
        let _ = run(&mut app, "server");
        assert!(matches!(app.mode, Mode::ServerPicker));
    }

    #[test]
    fn session_list_opens_picker_mode() {
        let mut app = fixture_app();
        let _ = run(&mut app, "session list");
        assert!(matches!(app.mode, Mode::SessionPicker));
    }

    #[test]
    fn session_alias_s_list_opens_picker_mode() {
        let mut app = fixture_app();
        let _ = run(&mut app, "s list");
        assert!(matches!(app.mode, Mode::SessionPicker));
    }

    #[test]
    fn pane_alias_p_new_dispatches() {
        // Without an active session, create_pane returns silently — but the
        // command must still resolve and run (no "unknown command" status).
        let mut app = fixture_app();
        let _ = run(&mut app, "p new");
        // No "unknown command" or parse error written to status.
        assert!(
            !app.mgr.status_msg().to_lowercase().contains("unknown"),
            "unexpected status: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn signal_with_no_active_pane_sets_error_status() {
        let mut app = fixture_app();
        let _ = run(&mut app, "signal kill");
        assert!(
            app.mgr.status_msg().contains("no active pane"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn signal_unknown_name_errors() {
        let mut app = fixture_app();
        let _ = run(&mut app, "signal foo");
        assert!(
            app.mgr.status_msg().contains("unknown signal"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn disconnected_session_new_reports_status() {
        let mut app = fixture_app();
        app.mgr.connected = false;
        let _ = run(&mut app, "session new");
        assert!(
            app.mgr.status_msg().contains("not connected"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn disconnected_signal_reports_status() {
        let mut app = fixture_app();
        app.mgr.connected = false;
        let _ = run(&mut app, "signal kill");
        assert!(
            app.mgr.status_msg().contains("not connected"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn pane_new_with_no_session_reports_status() {
        let mut app = fixture_app();
        // Connected, but no active session.
        let _ = run(&mut app, "pane new");
        assert!(
            app.mgr.status_msg().contains("no active session"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn unknown_command_writes_status() {
        let mut app = fixture_app();
        let _ = run(&mut app, "nopecommand");
        assert!(
            app.mgr.status_msg().contains("unknown command"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn rename_with_no_active_session_errors() {
        let mut app = fixture_app();
        let _ = run(&mut app, "session rename foo");
        assert!(
            app.mgr.status_msg().contains("no active session"),
            "got: {:?}",
            app.mgr.status_msg()
        );
    }

    #[test]
    fn local_returns_switch_server_outcome() {
        let mut app = fixture_app();
        assert!(matches!(
            run(&mut app, "local"),
            Outcome::SwitchServer(crate::app::SwitchTarget::Local)
        ));
    }

    #[test]
    fn reconnect_returns_reconnect_outcome() {
        let mut app = fixture_app();
        assert!(matches!(run(&mut app, "reconnect"), Outcome::Reconnect));
    }

    #[test]
    fn clear_history_drains_history() {
        let mut app = fixture_app();
        app.command_history.push_back("foo".into());
        app.command_history.push_back("bar".into());
        let _ = run(&mut app, "clear-history");
        assert_eq!(app.command_history.len(), 0);
    }

    /// Each canonical name must be unique within the registry.
    #[test]
    fn no_duplicate_canonical_names() {
        let mut names: Vec<&str> = ALL.iter().map(|s| s.name).collect();
        names.sort();
        let len = names.len();
        names.dedup();
        assert_eq!(len, names.len(), "duplicate canonical name in registry");
    }

    /// No two commands may share an alias or have an alias that collides with
    /// another command's canonical name.
    #[test]
    fn no_duplicate_aliases() {
        let mut all: Vec<&str> = Vec::new();
        for spec in ALL {
            all.push(spec.name);
            for a in spec.aliases {
                all.push(a);
            }
        }
        let len = all.len();
        all.sort();
        all.dedup();
        assert_eq!(len, all.len(), "duplicate name/alias across registry");
    }

    /// Usage strings should mention the canonical name and all required args.
    #[test]
    fn usage_strings_well_formed() {
        for spec in ALL {
            let u = spec.usage();
            assert!(u.starts_with('/'), "usage should start with /: {u}");
            assert!(u.contains(spec.name), "usage missing name: {u}");
            for a in spec.args {
                if a.required {
                    assert!(
                        u.contains(&format!("<{}>", a.name)),
                        "usage missing required <{}>: {u}",
                        a.name
                    );
                }
            }
        }
    }
}
