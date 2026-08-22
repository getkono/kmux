//! Measure what the ratchet ratchets: clippy diagnostics per crate, and the
//! `#[allow]` suppressions that would otherwise be a way around them.
//!
//! The gate consumes cargo's `--message-format=json` stream rather than parsing
//! human output, and re-prints each diagnostic's `rendered` field so the
//! developer still sees ordinary clippy output. Two details do real work:
//!
//! * **Deduplication.** cargo lints once per *target*, so a warning in
//!   `src/foo.rs` is reported again for the lib-test target that compiles the
//!   same file with `#[cfg(test)]` on. Counting raw messages double-counts
//!   every violation in a file with colocated tests, which in this repo is
//!   nearly all of them.
//! * **`--force-warn`, not `-W`.** `--force-warn` outranks both `-D warnings`
//!   (so a ratcheted lint stays a warning and gets counted instead of failing
//!   the compile) and any in-source `#[allow]` (so the budget cannot be paid
//!   down by suppressing rather than fixing). That second half is what makes
//!   the `[allows]` table a separate signal rather than a loophole.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// One diagnostic, reduced to the fields the gate reads.
#[derive(Debug, Deserialize)]
struct CargoMessage {
    reason: String,
    #[serde(default)]
    package_id: String,
    #[serde(default)]
    message: Option<Diagnostic>,
}

#[derive(Debug, Deserialize)]
struct Diagnostic {
    level: String,
    #[serde(default)]
    code: Option<Code>,
    #[serde(default)]
    rendered: Option<String>,
    #[serde(default)]
    spans: Vec<Span>,
}

#[derive(Debug, Deserialize)]
struct Code {
    code: String,
}

#[derive(Debug, Deserialize)]
struct Span {
    file_name: String,
    line_start: usize,
    column_start: usize,
    #[serde(default)]
    is_primary: bool,
}

/// The result of reading one clippy run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Measured {
    /// Violation counts keyed `crate/lint`.
    pub counts: BTreeMap<String, usize>,
    /// `file:line` for each violation, keyed the same way, so the gate can name
    /// the sites that broke a budget instead of only the number.
    pub sites: BTreeMap<String, Vec<String>>,
    /// Diagnostics at `error` level. These are the hard gate — lints declared
    /// in `[workspace.lints]` and promoted by `-D warnings` — and they fail
    /// regardless of any budget.
    pub errors: Vec<String>,
}

/// Extract the package name from a cargo package id.
///
/// Cargo emits two shapes depending on whether the directory name matches the
/// package name: `path+file:///…/crates/kmux-app#kmux-app@0.2.0` and
/// `path+file:///…/crates/kmux-app#0.2.0`. Both have to resolve to `kmux-app`,
/// because a crate whose directory disagrees with its name would otherwise get
/// its own budget rows and quietly leave the real ones stale.
pub fn package_name(package_id: &str) -> Option<String> {
    let (url, tail) = package_id.rsplit_once('#')?;
    if let Some((name, _version)) = tail.rsplit_once('@') {
        return Some(name.to_owned());
    }
    // No `name@version`: the fragment is a bare version, so the name is the
    // last path segment.
    if tail.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return url.rsplit('/').next().map(ToOwned::to_owned);
    }
    Some(tail.to_owned())
}

/// Parse a cargo JSON message stream.
///
/// Non-JSON lines and messages that are not diagnostics are skipped: cargo
/// interleaves build progress on the same stream, and a hard error there would
/// make the gate fail for reasons that have nothing to do with lints.
/// `rendered` text is written to `out` as it is read, so output ordering
/// matches an ordinary clippy run.
///
/// # Errors
/// If the stream cannot be read, or `out` cannot be written to.
pub fn measure(reader: impl std::io::BufRead, out: &mut impl std::io::Write) -> Result<Measured> {
    let mut measured = Measured::default();
    let mut seen: BTreeSet<(String, usize, usize, String)> = BTreeSet::new();

    for line in reader.lines() {
        let line = line.context("read the cargo message stream")?;
        let Ok(msg) = serde_json::from_str::<CargoMessage>(&line) else {
            continue;
        };
        if msg.reason != "compiler-message" {
            continue;
        }
        let Some(diag) = msg.message else { continue };
        if let Some(text) = &diag.rendered {
            write!(out, "{text}").context("re-print a clippy diagnostic")?;
        }
        let Some(code) = diag.code.as_ref().map(|c| c.code.clone()) else {
            continue;
        };
        let Some(span) = diag
            .spans
            .iter()
            .find(|s| s.is_primary)
            .or_else(|| diag.spans.first())
        else {
            continue;
        };
        // Absolute paths mean a dependency out of the registry, which the
        // workspace does not own and cannot fix.
        if Path::new(&span.file_name).is_absolute() {
            continue;
        }
        let key = (
            span.file_name.clone(),
            span.line_start,
            span.column_start,
            code.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        let site = format!("{}:{}", span.file_name, span.line_start);
        if diag.level == "error" {
            measured.errors.push(format!("{code} at {site}"));
            continue;
        }
        let Some(krate) = package_name(&msg.package_id) else {
            continue;
        };
        let bucket = format!("{krate}/{code}");
        *measured.counts.entry(bucket.clone()).or_default() += 1;
        measured.sites.entry(bucket).or_default().push(site);
    }
    Ok(measured)
}

/// Count `#[allow]` / `#![allow(` attributes per crate.
///
/// AGENTS.md requires a new suppression to be `#[expect(…, reason = "…")]`,
/// which the compiler deletes for you once it stops applying. `#[allow]` never
/// expires, so what is left of it is a debt with a number, and this is that
/// number. `#[expect]` is deliberately not counted.
///
/// # Errors
/// If a root cannot be walked or one of its files cannot be read.
pub fn count_allows(roots: &[&Path]) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for root in roots {
        for file in rust_files(root)? {
            let Some(krate) = crate_of(&file, root) else {
                continue;
            };
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let n = text.matches("#[allow(").count() + text.matches("#![allow(").count();
            if n > 0 {
                *counts.entry(krate).or_default() += n;
            }
        }
    }
    Ok(counts)
}

/// The crate a source file belongs to: the first path component under
/// `crates/`, or the root's own name for a single-crate root such as `xtask`.
fn crate_of(file: &Path, root: &Path) -> Option<String> {
    let rel = file.strip_prefix(root).ok()?;
    if root.file_name()? == "crates" {
        rel.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
    } else {
        Some(root.file_name()?.to_string_lossy().into_owned())
    }
}

/// Every `.rs` file under `root`, skipping build output.
fn rust_files(root: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("list {}", dir.display()))?;
        for entry in entries {
            let entry = entry.context("read a directory entry")?;
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The version of the `rustc` on PATH, as `x.y.z`.
///
/// The baseline records this so a toolchain upgrade cannot masquerade as a
/// regression — or, worse, absorb one, by changing what a lint fires on in the
/// same commit that changes the code.
///
/// # Errors
/// If `rustc` cannot be run, or its output cannot be parsed.
pub fn rustc_version() -> Result<String> {
    let out = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .context("run `rustc --version`")?;
    let text = String::from_utf8(out.stdout).context("decode `rustc --version` output")?;
    parse_rustc_version(&text).with_context(|| format!("parse `rustc --version` output: {text:?}"))
}

/// Pull `1.96.0` out of `rustc 1.96.0 (abcdef 2026-01-01)`.
pub fn parse_rustc_version(text: &str) -> Option<String> {
    text.split_whitespace().nth(1).map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_id_with_an_explicit_name_resolves_to_that_name() {
        assert_eq!(
            package_name("path+file:///w/crates/kmux-app#kmux-app@0.2.0").as_deref(),
            Some("kmux-app")
        );
    }

    #[test]
    fn a_package_id_with_a_bare_version_falls_back_to_the_directory() {
        assert_eq!(
            package_name("path+file:///w/crates/kmux-app#0.2.0").as_deref(),
            Some("kmux-app")
        );
    }

    #[test]
    fn a_package_id_with_no_fragment_has_no_name() {
        assert_eq!(package_name("path+file:///w/crates/kmux-app"), None);
    }

    #[test]
    fn a_rustc_version_line_yields_just_the_semver() {
        assert_eq!(
            parse_rustc_version("rustc 1.96.0 (abcdef012 2026-01-08)").as_deref(),
            Some("1.96.0")
        );
        assert_eq!(parse_rustc_version("").as_deref(), None);
    }

    fn diag(pkg: &str, file: &str, line: usize, col: usize, code: &str, level: &str) -> String {
        format!(
            r#"{{"reason":"compiler-message","package_id":"path+file:///w/crates/{pkg}#{pkg}@0.1.0","message":{{"level":"{level}","code":{{"code":"{code}"}},"rendered":"R","spans":[{{"file_name":"{file}","line_start":{line},"column_start":{col},"is_primary":true}}]}}}}"#
        )
    }

    fn run(lines: &[String]) -> (Measured, String) {
        let input = lines.join("\n");
        let mut out = Vec::new();
        let m = measure(std::io::Cursor::new(input), &mut out).expect("measure");
        (m, String::from_utf8(out).expect("utf8"))
    }

    #[test]
    fn violations_are_counted_per_crate_and_lint() {
        let (m, _) = run(&[
            diag(
                "kmux-app",
                "crates/kmux-app/src/a.rs",
                1,
                5,
                "clippy::x",
                "warning",
            ),
            diag(
                "kmux-app",
                "crates/kmux-app/src/a.rs",
                9,
                5,
                "clippy::x",
                "warning",
            ),
            diag(
                "kmuxd",
                "crates/kmuxd/src/b.rs",
                2,
                1,
                "clippy::x",
                "warning",
            ),
        ]);
        assert_eq!(m.counts["kmux-app/clippy::x"], 2);
        assert_eq!(m.counts["kmuxd/clippy::x"], 1);
        assert_eq!(
            m.sites["kmux-app/clippy::x"],
            vec!["crates/kmux-app/src/a.rs:1", "crates/kmux-app/src/a.rs:9"]
        );
    }

    #[test]
    fn the_same_site_reported_for_two_targets_counts_once() {
        // This is the lib and lib-test compilation of one file, which is what
        // `--all-targets` produces for every module with colocated tests.
        let one = diag(
            "kmux-app",
            "crates/kmux-app/src/a.rs",
            1,
            5,
            "clippy::x",
            "warning",
        );
        let (m, _) = run(&[one.clone(), one]);
        assert_eq!(m.counts["kmux-app/clippy::x"], 1);
    }

    #[test]
    fn two_lints_at_one_site_are_two_violations() {
        let (m, _) = run(&[
            diag(
                "kmux-app",
                "crates/kmux-app/src/a.rs",
                1,
                5,
                "clippy::x",
                "warning",
            ),
            diag(
                "kmux-app",
                "crates/kmux-app/src/a.rs",
                1,
                5,
                "clippy::y",
                "warning",
            ),
        ]);
        assert_eq!(m.counts["kmux-app/clippy::x"], 1);
        assert_eq!(m.counts["kmux-app/clippy::y"], 1);
    }

    #[test]
    fn errors_bypass_the_budget_entirely() {
        let (m, _) = run(&[diag(
            "kmux-app",
            "crates/kmux-app/src/a.rs",
            3,
            1,
            "clippy::hard",
            "error",
        )]);
        assert_eq!(m.errors, vec!["clippy::hard at crates/kmux-app/src/a.rs:3"]);
        assert!(
            m.counts.is_empty(),
            "an error must never become a budget row"
        );
    }

    #[test]
    fn diagnostics_in_dependencies_are_not_ours_to_count() {
        let (m, _) = run(&[diag(
            "somedep",
            "/home/u/.cargo/registry/src/x/lib.rs",
            1,
            1,
            "clippy::x",
            "warning",
        )]);
        assert!(m.counts.is_empty());
    }

    #[test]
    fn build_progress_and_junk_lines_are_skipped_not_fatal() {
        let (m, _) = run(&[
            r#"{"reason":"compiler-artifact","package_id":"p"}"#.to_owned(),
            "warning: something unstructured".to_owned(),
            String::new(),
            diag(
                "kmux-app",
                "crates/kmux-app/src/a.rs",
                1,
                5,
                "clippy::x",
                "warning",
            ),
        ]);
        assert_eq!(m.counts["kmux-app/clippy::x"], 1);
    }

    #[test]
    fn rendered_output_is_passed_through_so_clippy_still_reads_normally() {
        let (_, out) = run(&[diag(
            "kmux-app",
            "crates/kmux-app/src/a.rs",
            1,
            5,
            "clippy::x",
            "warning",
        )]);
        assert_eq!(out, "R");
    }

    #[test]
    fn a_diagnostic_without_a_lint_code_is_not_a_violation() {
        let line = r#"{"reason":"compiler-message","package_id":"path+file:///w/crates/a#a@0.1.0","message":{"level":"warning","code":null,"rendered":"R","spans":[]}}"#;
        let (m, out) = run(&[line.to_owned()]);
        assert!(m.counts.is_empty());
        assert_eq!(out, "R", "it is still shown to the developer");
    }

    #[test]
    fn a_file_under_crates_belongs_to_its_first_directory() {
        assert_eq!(
            crate_of(Path::new("crates/kmux-app/src/a.rs"), Path::new("crates")).as_deref(),
            Some("kmux-app")
        );
    }

    #[test]
    fn a_file_under_a_single_crate_root_belongs_to_that_root() {
        assert_eq!(
            crate_of(Path::new("xtask/src/a.rs"), Path::new("xtask")).as_deref(),
            Some("xtask")
        );
    }
}
