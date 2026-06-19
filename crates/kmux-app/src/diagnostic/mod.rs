//! Render diagnostic suite (issue #145).
//!
//! `kmux diagnostic <test>` opens the GUI with a *fresh, dedicated* session
//! running an automated emitter that paints a known terminal pattern (a glyph
//! grid, attribute matrix, color ramps, box-drawing…), so a human can visually
//! verify that the renderer-under-test draws it correctly. It complements the
//! [render-debug](super::core::render_debug) tooling, which shows what the
//! renderer was *handed*; this feeds it a *known input*.
//!
//! The emitter is the `kmux` binary itself in a hidden mode
//! (`kmux diagnostic <test> --emit`): it writes [`patterns::pattern_bytes`] to
//! stdout, then blocks on stdin so the pane stays visible. Both frontends resolve
//! the same launch command via [`session_command`], so the pattern bytes have a
//! single source of truth. Scope is the local daemon (the just-launched `kmux`
//! binary is present and locatable on the daemon host).

use std::path::PathBuf;

use clap::ValueEnum;

mod patterns;

pub use patterns::pattern_bytes;

/// A named diagnostic test pattern. The kebab-case name (`glyphs`, `attrs`, …)
/// is both the CLI value and the `--emit` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DiagnosticTest {
    /// ASCII + common Unicode glyphs (the primary "glyphs not rendered" repro).
    Glyphs,
    /// Text attributes (bold/italic/underline/…) across the four font faces.
    Attrs,
    /// 16-color, 256-color, and 24-bit truecolor ramps.
    Colors,
    /// Wide CJK, emoji, and combining marks (cell-width stress).
    Unicode,
    /// Box-drawing and line-drawing alignment grid.
    Boxes,
    /// Every pattern above, in order.
    All,
}

impl DiagnosticTest {
    /// The concrete patterns, in display order (excludes [`Self::All`], which is
    /// their concatenation).
    pub const EACH: [DiagnosticTest; 5] = [
        DiagnosticTest::Glyphs,
        DiagnosticTest::Attrs,
        DiagnosticTest::Colors,
        DiagnosticTest::Unicode,
        DiagnosticTest::Boxes,
    ];

    /// The kebab-case name used on the CLI and as the `--emit` argument.
    pub fn name(self) -> &'static str {
        match self {
            DiagnosticTest::Glyphs => "glyphs",
            DiagnosticTest::Attrs => "attrs",
            DiagnosticTest::Colors => "colors",
            DiagnosticTest::Unicode => "unicode",
            DiagnosticTest::Boxes => "boxes",
            DiagnosticTest::All => "all",
        }
    }

    /// Parse a kebab-case test name (the inverse of [`name`](Self::name)).
    /// Used by the Swift/FFI path, which carries the test as a plain string.
    pub fn from_name(name: &str) -> Option<DiagnosticTest> {
        DiagnosticTest::EACH
            .into_iter()
            .chain(std::iter::once(DiagnosticTest::All))
            .find(|test| test.name() == name)
    }

    /// One-line description for the `kmux diagnostic` catalogue.
    pub fn description(self) -> &'static str {
        match self {
            DiagnosticTest::Glyphs => "ASCII + common Unicode glyphs",
            DiagnosticTest::Attrs => "text attributes across the four font faces",
            DiagnosticTest::Colors => "16/256/truecolor ramps",
            DiagnosticTest::Unicode => "wide CJK, emoji, combining marks",
            DiagnosticTest::Boxes => "box-drawing alignment grid",
            DiagnosticTest::All => "run every pattern above, in order",
        }
    }
}

/// Print the available patterns (for `kmux diagnostic` with no test).
pub fn print_catalogue() {
    println!("Available diagnostic test patterns:\n");
    for test in DiagnosticTest::EACH {
        println!("  {:<8}  {}", test.name(), test.description());
    }
    println!(
        "  {:<8}  {}",
        DiagnosticTest::All.name(),
        DiagnosticTest::All.description()
    );
    println!("\nRun:  kmux diagnostic <test>");
}

/// Emit a pattern to stdout and hold the pane open until the user presses Enter
/// (or stdin reaches EOF). This is what the launched session runs; it is also
/// directly usable to test the *host* terminal.
pub fn emit(test: DiagnosticTest) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    let mut out = std::io::stdout().lock();
    out.write_all(&pattern_bytes(test))?;
    write!(
        out,
        "\n\x1b[7m Diagnostic: {} — press Enter to exit \x1b[0m\n",
        test.name()
    )?;
    out.flush()?;

    // Hold the pane open so the pattern stays on screen for inspection. The PTY
    // is line-buffered (cooked mode), so Enter delivers a newline; any EOF (the
    // pane being closed) also breaks the loop.
    let mut stdin = std::io::stdin().lock();
    let mut byte = [0u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' || byte[0] == b'\r' => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    Ok(())
}

/// The `(program, args)` that runs `test` in a session: the located `kmux`
/// binary invoked as `kmux diagnostic <test> --emit`. Shared by the GTK `Plan`
/// path and the Swift `build_core` path so both spawn an identical emitter.
pub fn session_command(test: DiagnosticTest) -> anyhow::Result<(String, Vec<String>)> {
    let kmux = locate_kmux_binary()?;
    Ok((
        kmux.to_string_lossy().into_owned(),
        vec![
            "diagnostic".to_string(),
            test.name().to_string(),
            "--emit".to_string(),
        ],
    ))
}

/// Locate the `kmux` entrypoint binary to run as the in-session emitter:
/// `KMUX_BIN` override → next to the running executable → on `PATH`. Mirrors the
/// sibling→`PATH` lookup the `kmux` entrypoint uses to find `kmux-gtk`.
///
/// Local-daemon scope: the resolved path is for *this* host, which is also the
/// daemon host, so it is valid where the session is spawned.
pub fn locate_kmux_binary() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("KMUX_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join("kmux");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("kmux");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "could not locate the `kmux` binary for the diagnostic session; \
         set KMUX_BIN or ensure `kmux` is installed alongside the GUI or on PATH"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_through_value_enum() {
        for test in DiagnosticTest::EACH {
            let parsed = DiagnosticTest::from_str(test.name(), false)
                .unwrap_or_else(|_| panic!("`{}` should parse", test.name()));
            assert_eq!(parsed, test);
        }
        assert_eq!(
            DiagnosticTest::from_str("all", false).unwrap(),
            DiagnosticTest::All
        );
    }

    #[test]
    fn session_command_passes_emit_flag() {
        // Make the locator deterministic regardless of the test host.
        unsafe { std::env::set_var("KMUX_BIN", std::env::current_exe().unwrap()) };
        let (_, args) = session_command(DiagnosticTest::Glyphs).unwrap();
        assert_eq!(args, vec!["diagnostic", "glyphs", "--emit"]);
        unsafe { std::env::remove_var("KMUX_BIN") };
    }
}
