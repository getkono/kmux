//! The `kmux` entrypoint — the unified command on Linux and macOS.
//!
//! It is deliberately thin and **toolkit-agnostic**: it shares the CLI front
//! door ([`kmux_app::launch::run_cli`]) with the frontends, so the
//! non-interactive subcommands (`daemon …`, `ls`, `--dry-run`) behave
//! identically everywhere without ever loading a UI toolkit. For an interactive
//! launch it hands off to the platform's desktop client by `exec`ing it:
//!
//! - **Linux:** the GTK frontend binary `kmux-gtk` (the default + official
//!   client), located next to this executable or on `PATH`.
//! - **macOS:** the native Swift app bundle (`~/Applications/kmux.app`,
//!   overridable with `KMUX_APP`).

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

/// Linux: exec the GTK frontend, forwarding our arguments.
#[cfg(target_os = "linux")]
fn launch_desktop() -> anyhow::Result<()> {
    exec_forwarding(locate_binary("kmux-gtk")?)
}

/// macOS: exec the native Swift app bundle's executable directly so it runs in
/// the foreground attached to the terminal (the Dock icon / app menu still come
/// from `Contents/Info.plist` above it), forwarding args + stdio. `KMUX_APP`
/// overrides the bundle location (defaults to `~/Applications/kmux.app`). This is
/// the Rust port of the former `kmux-swift/macos/kmux` launcher script.
#[cfg(target_os = "macos")]
fn launch_desktop() -> anyhow::Result<()> {
    let app = std::env::var_os("KMUX_APP")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_app_bundle);
    let exe = app.join("Contents/MacOS/kmux-swift");
    if !exe.is_file() {
        anyhow::bail!(
            "kmux: GUI not installed at {} — run `mise run install` from the kmux repo",
            exe.display()
        );
    }
    exec_forwarding(exe)
}

#[cfg(target_os = "macos")]
fn default_app_bundle() -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    home.join("Applications/kmux.app")
}

/// Other platforms have no supported desktop client.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn launch_desktop() -> anyhow::Result<()> {
    anyhow::bail!(
        "kmux: no desktop client for this platform; the GTK frontend (`kmux-gtk`) \
         is supported on Linux and macOS"
    )
}

/// Replace this process with `bin`, forwarding our arguments (minus argv[0]).
/// `exec` only returns on failure.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exec_forwarding(bin: std::path::PathBuf) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&bin)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(err).with_context(|| format!("failed to launch {}", bin.display()))
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
