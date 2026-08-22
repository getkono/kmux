//! Process-level setup: logging, the initial working directory, and building
//! the `AppCore` a driver wraps.

use super::*;

/// Install the client tracing subscriber the first time a driver is built, so
/// the Swift app's logs — this crate, `kmux-render`, and the rest of the client
/// stack — land in the client log file, exactly like the GTK/CLI front door's
/// `run_cli`. Without this the FFI path had no subscriber and dropped every
/// event, which is what made early GPU bugs undiagnosable (PR #144 review).
///
/// Guarded by `Once`: [`kmux_app::launch::init_logging`] sets the *global*
/// default subscriber, which must happen at most once per process (a second
/// `new` would otherwise panic). Honors `RUST_LOG` / `KMUX_LOG_STDERR`.
pub(crate) fn init_ffi_logging(instance_id: &str) {
    static FFI_LOGGING: Once = Once::new();
    FFI_LOGGING.call_once(|| kmux_app::launch::init_logging(instance_id));
}

/// Resolve the startup directory for the native GUI.
///
/// Unlike a CLI process, an app bundle is not launched from a meaningful shell
/// directory (macOS commonly gives it `/`). New GUI sessions therefore start
/// in the user's home directory. Keep a current-directory fallback for unusual
/// environments without `HOME`; explicit launch paths remain `auto_cwd` and
/// take precedence later in [`AppCore::auto_select_session`].
pub(crate) fn gui_initial_cwd() -> String {
    select_gui_initial_cwd(
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        std::env::current_dir().ok().as_deref(),
    )
}

pub(crate) fn select_gui_initial_cwd(home: Option<&Path>, current_dir: Option<&Path>) -> String {
    home.or(current_dir)
        .and_then(Path::to_str)
        .unwrap_or_default()
        .to_string()
}

/// Build an [`AppCore`] from a [`DriverConfig`], resolving the server target and
/// theme exactly as `kmux_app::launch::run_cli` does for the Rust frontends.
pub(crate) fn build_core(config: &DriverConfig, instance_id: String) -> AppCore {
    let (target, parsed_server) = parse_target(config.server.as_deref(), config.ssh_port);
    let auto_cwd = config
        .cwd
        .clone()
        .or_else(|| parsed_server.as_ref().and_then(|p| p.path.clone()));
    let theme = config::resolve_theme(config.theme.as_deref());
    // No `--font` flag on the Swift path; the appearance resolves from
    // `config.toml` (mirroring how `theme`/`cursor_blink` default here).
    let appearance = config::resolve_appearance(None);
    let cursor_blink = config::resolve_cursor_blink(config.cursor_blink);
    let initial_cwd = gui_initial_cwd();
    // `kmux diagnostic <test>` on macOS: the Swift app forwards the test name
    // here; resolve it to the same emitter command the GTK path uses (issue
    // #145). An unknown name or a missing `kmux` binary degrades to an ordinary
    // shell launch rather than failing the driver.
    let initial_program = config.diagnostic.as_deref().and_then(|name| {
        let test = DiagnosticTest::from_name(name)?;
        match diagnostic::session_command(test) {
            Ok(cmd) => Some(cmd),
            Err(e) => {
                tracing::warn!(error = %e, test = name, "diagnostic launch unavailable; opening a shell");
                None
            }
        }
    });
    // GUI capabilities: truecolor on, no kitty keyboard/graphics concept.
    let capabilities = ClientCapabilities {
        truecolor: true,
        kitty_graphics: false,
        kitty_keyboard: false,
        term: None,
        term_program: Some("kmux-macos".to_string()),
    };
    let term_size = TermSize {
        rows: config.rows,
        cols: config.cols,
        pixel_width: config.pixel_width,
        pixel_height: config.pixel_height,
    };
    AppCore::new(
        target,
        initial_cwd,
        instance_id,
        config.session.clone(),
        auto_cwd,
        initial_program,
        capabilities,
        theme,
        appearance,
        cursor_blink,
        term_size,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::select_gui_initial_cwd;

    #[test]
    fn gui_initial_cwd_prefers_home_over_process_directory() {
        assert_eq!(
            select_gui_initial_cwd(Some(Path::new("/Users/alice")), Some(Path::new("/"))),
            "/Users/alice"
        );
    }

    #[test]
    fn gui_initial_cwd_falls_back_to_process_directory_without_home() {
        assert_eq!(
            select_gui_initial_cwd(None, Some(Path::new("/work/project"))),
            "/work/project"
        );
    }

    #[test]
    fn gui_initial_cwd_is_empty_without_a_resolvable_directory() {
        assert_eq!(select_gui_initial_cwd(None, None), "");
    }
}
