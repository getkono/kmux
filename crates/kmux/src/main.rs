//! The `kmux` entrypoint — the unified command on Linux and macOS.
//!
//! It is deliberately thin and **toolkit-agnostic**: it shares the CLI front
//! door ([`kmux_app::launch::run_cli`]) with the frontends, so the
//! non-interactive subcommands (`daemon …`, `ls`, `--dry-run`) behave
//! identically everywhere without ever loading a UI toolkit. For an interactive
//! launch it hands off to the platform's desktop client:
//!
//! - **Linux:** `exec`s the GTK frontend binary `kmux-gtk` (the default +
//!   official client), located next to this executable or on `PATH`.
//! - **macOS:** launches the native Swift app bundle (`~/Applications/kmux.app`,
//!   overridable with `KMUX_APP`) via `open` so each invocation gets its own
//!   window in the singleton instance.
//!
//! Prod vs dev split (see [`launch_desktop`]): a **release** build is the native
//! singleton — it routes to / cold-starts the installed GUI (macOS LaunchServices
//! plus the `kmux://` URL scheme; Linux D-Bus app-id). A **debug** build
//! (`./kmux`, the `dev` mise task) instead **kills any stale instance and runs
//! the freshly built binary directly**, so what you see is always the code you
//! just compiled — never a handoff to an outdated GUI. On macOS the dev binary is
//! the bare `kmux-swift` (`KMUX_APP` is a file, not a `.app`); on Linux `kmux-gtk`
//! runs `NON_UNIQUE` in debug.

use kmux_app::launch::{Launch, run_cli};
use kmux_client::generate_instance_id;

fn main() -> anyhow::Result<()> {
    // A tokio runtime backs `run_cli`'s async daemon/subcommand network calls.
    let rt = tokio::runtime::Runtime::new()?;
    match rt.block_on(run_cli(generate_instance_id()))? {
        // A non-interactive subcommand (daemon / ls / --dry-run) handled everything.
        Launch::Done => Ok(()),
        // Interactive: hand off to the platform desktop client. We discard the
        // resolved `Plan` and forward argv instead — the spawned frontend re-runs
        // `run_cli` and rebuilds the identical plan (a benign double-parse). This
        // keeps each frontend runnable standalone with the very same flags.
        Launch::Interactive(_plan) => {
            // Warn before nesting a GUI inside a kmux-managed shell (#73). The
            // check lives only here in the entrypoint (not in the shared
            // `run_cli`), so the frontend we exec below never re-prompts.
            if !nested_kmux_check()? {
                return Ok(());
            }
            launch_desktop()
        }
    }
}

/// If launched from inside a kmux-managed shell (the daemon exports `KMUX` in
/// every pane), warn the user: opening a kmux GUI nested inside kmux is usually
/// a mistake, and the new window is invisible on a headless host. Returns
/// whether to proceed with the launch (issue #73).
fn nested_kmux_check() -> anyhow::Result<bool> {
    use std::io::{IsTerminal, Write};

    // Not nested, or the user permanently opted out → proceed silently.
    if std::env::var_os("KMUX").is_none() || !kmux_app::config::warn_when_nested() {
        return Ok(true);
    }

    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "\n\x1b[33m⚠  This shell is already running under kmux (KMUX is set).\x1b[0m"
    );
    let _ = writeln!(
        err,
        "   Opening another kmux here nests a multiplexer inside itself, and the"
    );
    let _ = writeln!(err, "   new window is invisible on a headless host.\n");

    // No TTY (non-interactive / headless): we cannot ask, and a nested invisible
    // GUI is almost certainly unwanted — refuse, but say how to override.
    if !std::io::stdin().is_terminal() {
        let _ = writeln!(
            err,
            "   Refusing to start non-interactively. Set `warn_nested = false` in"
        );
        let _ = writeln!(err, "   ~/.config/kmux/config.toml to allow it.\n");
        return Ok(false);
    }

    loop {
        let _ = write!(
            err,
            "   [d] don't start   [s] start anyway   [a] always start from now on   [d]? "
        );
        let _ = err.flush();
        let mut line = String::new();
        // EOF (Ctrl-D) → treat as the safe default: don't start.
        if std::io::stdin().read_line(&mut line)? == 0 {
            let _ = writeln!(err);
            return Ok(false);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "d" | "n" | "no" => return Ok(false),
            "s" | "y" | "yes" => return Ok(true),
            "a" | "always" => {
                if let Err(e) = kmux_app::config::set_warn_when_nested(false) {
                    let _ = writeln!(err, "   (couldn't save preference: {e})");
                }
                return Ok(true);
            }
            other => {
                let _ = writeln!(
                    err,
                    "   Unrecognized choice '{other}'. Please enter d, s, or a."
                );
            }
        }
    }
}

/// Linux: exec the GTK frontend, forwarding our arguments. A release build is a
/// D-Bus singleton (a second `kmux` opens a new window in the running instance).
/// A debug build kills any prior dev instance first and `kmux-gtk` runs
/// `NON_UNIQUE` (see `imp::run_gui`), so `./kmux` always runs the freshly built
/// code rather than handing off to a possibly-stale primary.
#[cfg(target_os = "linux")]
fn launch_desktop() -> anyhow::Result<()> {
    let bin = locate_binary("kmux-gtk")?;
    if cfg!(debug_assertions) {
        kill_running(&bin);
    }
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    exec_forwarding(&bin, &args)
}

/// macOS: launch the native Swift app bundle via `open` so each `kmux`
/// invocation gets its **own window**. When an instance is already running, the
/// launch is routed to it through the `kmux://` URL scheme (it opens a new
/// window for the URL); otherwise the app is cold-started with the same request
/// forwarded on argv. Going through `open` (LaunchServices) — rather than a bare
/// exec of the bundle binary — is what makes the running-instance routing work,
/// fixing repeated launches collapsing into one window. `KMUX_APP` overrides the
/// bundle location (defaults to `~/Applications/kmux.app`).
///
/// Dev exception: when `KMUX_APP` points at a regular file (the freshly built
/// `kmux-swift` executable, not a `.app` directory — the layout the `dev` mise
/// task sets up), `exec` it directly in the foreground, forwarding the same
/// `--launch-url`. That keeps its stdio attached to the launching terminal so
/// logs/backtraces stream there (what `./kmux` relies on), and routes the dev
/// GUI through this very entrypoint instead of a parallel `swift run` path.
#[cfg(target_os = "macos")]
fn launch_desktop() -> anyhow::Result<()> {
    use clap::Parser;

    let app = std::env::var_os("KMUX_APP")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_app_bundle);

    let url = build_launch_url(&kmux_app::cli::Cli::parse());

    if app.is_file() {
        // Dev path: `KMUX_APP` is the freshly built bare `kmux-swift` (the `dev`
        // mise task sets it). Kill any instance already running that exact binary,
        // then `exec` the fresh one in the foreground (stdio attached for logs).
        // This *guarantees* `./kmux` shows the build you just compiled — a bare
        // exec never coalesces with the prod bundle, and the kill replaces a stale
        // dev instance instead of leaving a confusing second one.
        kill_running(&app);
        return exec_forwarding(
            &app,
            &[
                std::ffi::OsString::from("--launch-url"),
                std::ffi::OsString::from(&url),
            ],
        );
    }

    let exe = app.join("Contents/MacOS/kmux-swift");
    if !exe.is_file() {
        anyhow::bail!(
            "kmux: GUI not installed at {} — run `mise run install` from the kmux repo",
            exe.display()
        );
    }

    if swift_app_running() {
        // Running: route to the existing instance → it opens a new window.
        run_open(&[std::ffi::OsString::from(&url)])
    } else {
        // Cold start: launch the app with the request forwarded on argv.
        run_open(&[
            app.into_os_string(),
            std::ffi::OsString::from("--args"),
            std::ffi::OsString::from("--launch-url"),
            std::ffi::OsString::from(&url),
        ])
    }
}

/// Run `/usr/bin/open` with `args`, erroring if it fails to launch.
#[cfg(target_os = "macos")]
fn run_open(args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    use anyhow::Context;
    let status = std::process::Command::new("/usr/bin/open")
        .args(args)
        .status()
        .context("failed to run `open` to launch the kmux app")?;
    if !status.success() {
        anyhow::bail!("`open` exited unsuccessfully ({status})");
    }
    Ok(())
}

/// Whether an instance of the Swift app (`kmux-swift`) is already running, so a
/// new launch should route to it (a new window) rather than cold-start. On any
/// uncertainty this returns `false` (cold start), which is always safe.
#[cfg(target_os = "macos")]
fn swift_app_running() -> bool {
    std::process::Command::new("/usr/bin/pgrep")
        .args(["-x", "kmux-swift"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build the `kmux://new?…` URL the app turns into one window's `LaunchRequest`
/// (server / ssh-port / session / cwd / diagnostic), re-parsing the shared `Cli`.
#[cfg(target_os = "macos")]
fn build_launch_url(cli: &kmux_app::cli::Cli) -> String {
    // `kmux open <host>` carries the connection args on the `Open` variant, while
    // the bare `kmux <host>` carries them on the top-level flattened args. Both
    // must produce the same URL, so source from whichever applies.
    let connect = match &cli.command {
        Some(kmux_app::cli::Command::Open { connect }) => connect,
        _ => &cli.connect,
    };
    let mut params: Vec<(&str, String)> = Vec::new();
    if let Some(s) = &connect.server_args.server {
        params.push(("server", s.clone()));
    }
    if let Some(p) = connect.server_args.ssh_port {
        params.push(("ssh-port", p.to_string()));
    }
    if let Some(s) = &connect.session {
        params.push(("session", s.clone()));
    }
    if let Some(c) = &connect.cwd {
        params.push(("cwd", c.clone()));
    }
    if let Some(kmux_app::cli::Command::Diagnostic {
        test: Some(t),
        emit: false,
    }) = &cli.command
        && let Some(v) = clap::ValueEnum::to_possible_value(t)
    {
        params.push(("diagnostic", v.get_name().to_string()));
    }
    if params.is_empty() {
        return "kmux://new".to_string();
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("kmux://new?{query}")
}

/// Percent-encode a URL query value, keeping the RFC 3986 unreserved set verbatim.
#[cfg(target_os = "macos")]
fn percent_encode(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn default_app_bundle() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    home.join("Applications/kmux.app")
}

/// Other platforms have no supported desktop client yet.
///
/// Windows is not built. When it is, its singleton model must match the Unix
/// one: a per-profile single-instance guard on top of the per-profile runtime
/// dir that already isolates debug from release ([`kmux_protocol::dirs`]). The
/// intended mechanism is a named mutex — `CreateMutexW` on a name that embeds the
/// build profile (e.g. `Local\\kmux-{profile}`): the first launcher creates it
/// and owns the window; a second sees `ERROR_ALREADY_EXISTS` and forwards its
/// request to the running instance (the same handoff the GTK D-Bus `activate` /
/// Swift `kmux://` routing perform today) rather than opening a rival process.
/// See `docs/architecture-frontend.md`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn launch_desktop() -> anyhow::Result<()> {
    anyhow::bail!(
        "kmux: no desktop client for this platform yet; the GTK frontend \
         (`kmux-gtk`) is supported on Linux and macOS"
    )
}

/// Replace this process with `bin`, passing `args`. `exec` only returns on
/// failure, so on success this never returns and the frontend inherits our
/// stdio (its output streams to the launching terminal). Used by Linux (the
/// `kmux-gtk` handoff, forwarding our argv) and by the macOS dev path (`exec`ing
/// the bare `kmux-swift` executable with `--launch-url`); macOS prod launches
/// via `open` instead (see [`launch_desktop`]) so it can route to a running
/// instance.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exec_forwarding(bin: &std::path::Path, args: &[std::ffi::OsString]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(bin).args(args).exec();
    Err(err).with_context(|| format!("failed to launch {}", bin.display()))
}

/// Terminate any process already running the exact executable at `bin`, so a dev
/// launch *replaces* a stale instance instead of leaving two. Best-effort and
/// dev-only: `bin` is the freshly built GUI binary, matched by full path via
/// `pkill -f` — our own `kmux` process never matches (its argv doesn't contain
/// `bin`), and the installed prod bundle lives at a different path. The short
/// sleep lets the old instance release its window/daemon connection before the
/// fresh one starts.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn kill_running(bin: &std::path::Path) {
    let Some(path) = bin.to_str() else { return };
    let _ = std::process::Command::new("pkill")
        .args(["-f", path])
        .status();
    std::thread::sleep(std::time::Duration::from_millis(150));
}

/// Find a sibling binary `name`: next to the running executable first (the
/// installed / `target/<profile>` layout), then on `PATH`. Mirrors
/// `kmux_client`'s `find_server_binary` for locating `kmuxd`.
#[cfg(target_os = "linux")]
fn locate_binary(name: &str) -> anyhow::Result<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "could not find the `{name}` binary; ensure it is installed alongside `kmux` or on PATH"
    )
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use clap::Parser;

    /// `kmux open <host>` must build the same launch URL as the bare positional —
    /// the host lives on the `Open` variant, so `build_launch_url` has to read it
    /// from there (regression guard for the cross-frontend launch path).
    #[test]
    fn open_subcommand_url_includes_server() {
        let cli = kmux_app::cli::Cli::parse_from(["kmux", "open", "host"]);
        assert_eq!(build_launch_url(&cli), "kmux://new?server=host");
    }

    #[test]
    fn bare_positional_url_includes_server() {
        let cli = kmux_app::cli::Cli::parse_from(["kmux", "host"]);
        assert_eq!(build_launch_url(&cli), "kmux://new?server=host");
    }
}
