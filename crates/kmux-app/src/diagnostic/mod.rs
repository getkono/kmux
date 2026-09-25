//! Render diagnostic suite (issue #145).
//!
//! `kmux diagnostic <test>` opens the GUI with a *fresh, dedicated* session
//! running an automated emitter that paints a known terminal pattern (a glyph
//! grid, attribute matrix, color ramps, box-drawing…), so a human can visually
//! verify that the renderer-under-test draws it correctly. It complements the
//! [render-debug](crate::core::RenderDebugSnapshot) tooling, which shows what the
//! renderer was *handed*; this feeds it a *known input*.
//!
//! The emitter is the `kmux` binary itself in a hidden mode
//! (`kmux diagnostic <test> --emit`): it writes [`crate::diagnostic::pattern_bytes`] to
//! stdout, then blocks on stdin so the pane stays visible. Both frontends resolve
//! the same launch command via [`crate::diagnostic::session_command`], so the pattern bytes have a
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
    /// Animated OSC 9;4 (ConEmu/WT) progress-bar states (issue #125). Unlike the
    /// in-grid patterns this paints *window chrome* and loops over time, so it is
    /// excluded from [`Self::All`].
    Progress,
    /// Every pattern above, in order.
    All,
}

impl DiagnosticTest {
    /// The concrete patterns, in display order (excludes [`Self::All`], which is
    /// their concatenation).
    pub const EACH: [Self; 5] = [
        Self::Glyphs,
        Self::Attrs,
        Self::Colors,
        Self::Unicode,
        Self::Boxes,
    ];

    /// The kebab-case name used on the CLI and as the `--emit` argument.
    pub fn name(self) -> &'static str {
        match self {
            Self::Glyphs => "glyphs",
            Self::Attrs => "attrs",
            Self::Colors => "colors",
            Self::Unicode => "unicode",
            Self::Boxes => "boxes",
            Self::Progress => "progress",
            Self::All => "all",
        }
    }

    /// Parse a kebab-case test name (the inverse of [`name`](Self::name)).
    /// Used by the Swift/FFI path, which carries the test as a plain string.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::EACH
            .into_iter()
            .chain([Self::Progress, Self::All])
            .find(|test| test.name() == name)
    }

    /// One-line description for the `kmux diagnostic` catalogue.
    pub fn description(self) -> &'static str {
        match self {
            Self::Glyphs => "ASCII + common Unicode glyphs",
            Self::Attrs => "text attributes across the four font faces",
            Self::Colors => "16/256/truecolor ramps",
            Self::Unicode => "wide CJK, emoji, combining marks",
            Self::Boxes => "box-drawing alignment grid",
            Self::Progress => "animated OSC 9;4 progress-bar states",
            Self::All => "run every pattern above, in order",
        }
    }
}

/// Print the available patterns (for `kmux diagnostic` with no test).
pub fn print_catalogue() {
    println!("Available diagnostic test patterns:\n");
    for test in DiagnosticTest::EACH {
        println!("  {:<8}  {}", test.name(), test.description());
    }
    for test in [DiagnosticTest::Progress, DiagnosticTest::All] {
        println!("  {:<8}  {}", test.name(), test.description());
    }
    println!("\nRun:  kmux diagnostic <test>");
}

/// Emit a pattern to stdout and hold the pane open until the user presses Enter
/// (or stdin reaches EOF). This is what the launched session runs; it is also
/// directly usable to test the *host* terminal.
pub fn emit(test: DiagnosticTest) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    // The progress test paints window chrome, not the grid, and is inherently
    // animated — it runs its own timed loop instead of the one-shot path below.
    if test == DiagnosticTest::Progress {
        return emit_progress_animated();
    }

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

/// Animated emitter for the `progress` test (OSC 9;4 / issue #125). The progress
/// bar is window chrome, not grid content, and is stateful, so this loops over
/// the states — sweeping `0→100` for the numeric ones (set/error/pause), holding
/// the value-less indeterminate, then clearing — narrating each step in the pane
/// so the viewer knows the expected bar. Holds the pane open: the loop runs until
/// stdin reaches EOF or the user presses Enter (a reader thread flips `stop`).
///
/// `KMUX_DIAG_PROGRESS_STEP_MS` overrides the per-step delay (default 200 ms);
/// the integration test sets it to `0` for a fast, deterministic run.
fn emit_progress_animated() -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let step = Duration::from_millis(
        std::env::var("KMUX_DIAG_PROGRESS_STEP_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(200),
    );

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "\x1b[1mOSC 9;4 progress diagnostic (issue #125)\x1b[0m"
    )?;
    writeln!(
        out,
        "Watch the pane's progress bar. Cycling states: \
         1=set 2=error 4=pause 3=indeterminate 0=remove."
    )?;
    out.flush()?;

    // A reader thread flips `stop` on Enter or EOF (pane closed / stdin closed),
    // so the animation loop is interruptible without blocking on stdin itself.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
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
            stop.store(true, Ordering::SeqCst);
        });
    }

    // The numeric states that carry a percentage, with the colour to expect.
    let sweeps: [(u8, &str); 3] = [(1, "accent bar"), (2, "red bar"), (4, "amber bar")];
    'cycle: loop {
        for (state, expect) in sweeps {
            for pct in (0..=100).step_by(20) {
                // Write the frame *before* checking `stop`, so a fast EOF (e.g.
                // the integration test's closed stdin) still produces at least
                // one OSC 9;4 sequence rather than racing the reader thread.
                writeln!(out, "OSC 9;4;{state};{pct} — expect {expect} at {pct}%")?;
                write!(out, "\x1b]9;4;{state};{pct}\x07")?;
                out.flush()?;
                if stop.load(Ordering::SeqCst) {
                    break 'cycle;
                }
                sleep_unless_stopped(&stop, step);
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        writeln!(out, "OSC 9;4;3 — expect an indeterminate/busy bar (no %)")?;
        write!(out, "\x1b]9;4;3\x07")?;
        out.flush()?;
        sleep_unless_stopped(&stop, step * 5);

        if stop.load(Ordering::SeqCst) {
            break;
        }
        writeln!(out, "OSC 9;4;0 — expect the bar to disappear")?;
        write!(out, "\x1b]9;4;0\x07")?;
        out.flush()?;
        sleep_unless_stopped(&stop, step * 5);
    }

    // Leave no bar behind on exit.
    let _ = write!(out, "\x1b]9;4;0\x07");
    let _ = out.flush();
    Ok(())
}

/// Sleep `dur`, but wake early (in ~20 ms slices) if `stop` is set, so Enter/EOF
/// ends the animation promptly. A zero duration returns immediately.
fn sleep_unless_stopped(stop: &std::sync::atomic::AtomicBool, dur: std::time::Duration) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let slice = Duration::from_millis(20);
    let mut remaining = dur;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let s = remaining.min(slice);
        std::thread::sleep(s);
        remaining -= s;
    }
}

/// The `(program, args)` that runs `test` in a session: the located `kmux`
/// binary invoked as `kmux diagnostic <test> --emit`. Shared by the GTK `Plan`
/// path and the Swift `build_core` path so both spawn an identical emitter.
pub fn session_command(test: DiagnosticTest) -> anyhow::Result<(String, Vec<String>)> {
    Ok(session_command_for(&locate_kmux_binary()?, test))
}

/// The argv [`session_command`] builds, for an already-located binary.
///
/// Split out so the interesting half — which flags a given test emits — is
/// testable without a locator that reads `KMUX_BIN` and the filesystem. See
/// docs/testing.md R3.
#[must_use]
pub fn session_command_for(kmux: &std::path::Path, test: DiagnosticTest) -> (String, Vec<String>) {
    (
        kmux.to_string_lossy().into_owned(),
        vec![
            "diagnostic".to_string(),
            test.name().to_string(),
            "--emit".to_string(),
        ],
    )
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
        // `progress` and `all` are not in EACH but must still parse.
        assert_eq!(
            DiagnosticTest::from_str("progress", false).unwrap(),
            DiagnosticTest::Progress
        );
        assert_eq!(
            DiagnosticTest::from_str("all", false).unwrap(),
            DiagnosticTest::All
        );
    }

    #[test]
    fn from_name_resolves_progress() {
        assert_eq!(
            DiagnosticTest::from_name("progress"),
            Some(DiagnosticTest::Progress)
        );
    }

    #[test]
    fn progress_is_excluded_from_all() {
        // `All` concatenates the in-grid `EACH` patterns; the animated progress
        // chrome test must not be swept into it.
        assert!(!DiagnosticTest::EACH.contains(&DiagnosticTest::Progress));
    }

    #[test]
    fn session_command_passes_emit_flag() {
        let kmux = std::path::Path::new("/opt/kmux/bin/kmux");
        let (bin, args) = session_command_for(kmux, DiagnosticTest::Glyphs);
        assert_eq!(bin, "/opt/kmux/bin/kmux");
        assert_eq!(args, vec!["diagnostic", "glyphs", "--emit"]);

        let (_, args) = session_command_for(kmux, DiagnosticTest::Progress);
        assert_eq!(args, vec!["diagnostic", "progress", "--emit"]);
    }
}
