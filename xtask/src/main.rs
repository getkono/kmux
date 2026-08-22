//! Command-line entrypoint for the workspace quality tooling. See `lib.rs` for
//! why this crate exists and where it lives.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use xtask::baseline::{self, Baseline, Finding, compare};
use xtask::graph::Graph;
use xtask::{lint, mutants};

/// Roots scanned for `#[allow]` attributes: the product crates and this crate.
/// Tooling holds itself to the same bar as the code it gates.
const ALLOW_ROOTS: [&str; 2] = ["crates", "xtask"];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("deps-graph") => print_graph(),
        Some("lint-flags") => print_lint_flags(),
        Some("lint-gate") => lint_gate(args.next().as_deref() == Some("--write")),
        Some("mutants-gate") => {
            let rest: Vec<String> = args.collect();
            let write = rest.iter().any(|a| a == "--write");
            let dirs: Vec<PathBuf> = rest
                .iter()
                .filter(|a| !a.starts_with("--"))
                .map(PathBuf::from)
                .collect();
            mutants_gate(&dirs, write)
        }
        Some(other) => bail!("unknown command `{other}`; known commands: {COMMANDS}"),
        None => {
            eprintln!("usage: cargo run -p xtask -- <command>");
            eprintln!();
            eprintln!("commands:");
            eprintln!(
                "  deps-graph          print the workspace dependency graph as it is asserted on"
            );
            eprintln!("  lint-flags          print the --force-warn flags for the ratcheted lints");
            eprintln!(
                "  lint-gate [--write] compare a clippy JSON stream on stdin against the baseline"
            );
            bail!("no command given")
        }
    }
}

const COMMANDS: &str = "deps-graph, lint-flags, lint-gate, mutants-gate";

/// The checked-in budget, resolved relative to the workspace root.
fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or(Path::new("."))
        .join("quality-baseline.toml")
}

/// Print the internal dependency graph. This is the debugging counterpart to
/// the assertions in `tests/dependency_direction.rs`: when one fails, this shows
/// the graph it was reading.
fn print_graph() -> Result<()> {
    let g = Graph::load()?;
    for member in &g.members {
        let internal: Vec<String> = g
            .reachable_from(member)
            .into_iter()
            .filter(|d| g.members.contains(d))
            .collect();
        println!("{member}");
        if internal.is_empty() {
            println!("    (no internal dependencies)");
        } else {
            println!("    {}", internal.join(", "));
        }
    }
    Ok(())
}

/// Emit the `--force-warn` flags for every ratcheted lint, so the lint list has
/// exactly one home (the baseline's `[meta].ratcheted`) and the mise task that
/// runs clippy does not carry a second copy that can drift out of step.
fn print_lint_flags() -> Result<()> {
    let baseline = Baseline::load(&baseline_path())?;
    // One token per line, `--force-warn=<lint>` rather than two words: the
    // caller reads them into a shell array, and a two-word form would make the
    // whole list depend on the caller's word splitting -- which differs between
    // shells and fails by passing the entire list to rustc as one argument.
    for lint in &baseline.meta.ratcheted {
        println!("--force-warn={lint}");
    }
    Ok(())
}

/// Read a clippy JSON stream from stdin and hold it against the baseline.
fn lint_gate(write: bool) -> Result<()> {
    let path = baseline_path();
    let baseline = Baseline::load(&path)?;

    let mut stdout = std::io::stdout().lock();
    let measured = lint::measure(std::io::stdin().lock(), &mut stdout)?;
    stdout
        .flush()
        .context("flush the re-printed clippy output")?;

    let allows = lint::count_allows(
        &ALLOW_ROOTS
            .iter()
            .map(|r| baseline_path().parent().unwrap_or(Path::new(".")).join(r))
            .collect::<Vec<_>>()
            .iter()
            .map(PathBuf::as_path)
            .collect::<Vec<_>>(),
    )?;

    if write {
        // A partial clippy run measures a subset of the tree, and writing that
        // as a baseline records budgets that are too LOOSE — the exact failure
        // this whole gate exists to make impossible. One hard-lint error stops
        // cargo before the crates downstream of it are ever linted, so refuse.
        if !measured.errors.is_empty() {
            for err in &measured.errors {
                eprintln!("baseline: hard lint: {err}");
            }
            bail!(
                "refusing to write a baseline from a run with {} hard-lint error(s): \
                 cargo stops at the first failing crate, so everything downstream \
                 of it went unmeasured. Fix these, then re-run.",
                measured.errors.len()
            );
        }
        // Same refusal for the other way a run comes back empty: no error, no
        // diagnostics, because cargo replayed a cache the gate's flags never
        // populated. Writing that records zeros for a crate nobody measured.
        let unmeasured = baseline::implausibly_clear(&baseline.lint_budgets(), &measured.counts);
        if !unmeasured.is_empty() {
            for u in &unmeasured {
                eprintln!("baseline: {u}");
            }
            bail!(
                "refusing to write a baseline in which {} crate(s) reported nothing at all.",
                unmeasured.len()
            );
        }
        return rewrite(&path, &baseline, &measured.counts, &allows);
    }

    let rustc = lint::rustc_version()?;
    let mut failures: Vec<String> = Vec::new();

    if rustc != baseline.meta.rustc {
        failures.push(format!(
            "toolchain changed: baseline was measured on rustc {}, this is rustc {rustc}.\n  \
             A compiler upgrade changes what fires, so re-measure deliberately \
             (`mise run baseline`) in a commit of its own.",
            baseline.meta.rustc
        ));
    }

    // Hard-gate violations: lints the workspace already holds at zero, so no
    // budget applies and nothing about the ratchet can excuse them.
    for err in &measured.errors {
        failures.push(format!("hard lint: {err}"));
    }

    // Budgets are only compared when the run finished. cargo stops at the first
    // crate that fails, so every crate downstream of a hard-lint error goes
    // unlinted and reads as zero violations — which the comparison would report
    // as a dozen "stale budget" failures that vanish the moment the real error
    // is fixed. Reporting those alongside the actual cause is worse than not
    // reporting them: it buries it.
    // Believability before budget, the same order the mutation gate uses. A
    // crate that reports nothing at all against a large budget was not measured;
    // saying so beats twenty "stale budget" lines that invite someone to record
    // zeros.
    let unmeasured = if measured.errors.is_empty() {
        baseline::implausibly_clear(&baseline.lint_budgets(), &measured.counts)
    } else {
        // A hard-lint error already explains every crate downstream of it
        // reading as zero; repeating that as "unmeasured" would bury the one
        // line that matters.
        Vec::new()
    };
    for u in &unmeasured {
        failures.push(format!("unmeasured: {u}"));
    }

    if measured.errors.is_empty() && unmeasured.is_empty() {
        report(
            &mut failures,
            "lint",
            &compare(&baseline.lint_budgets(), &measured.counts),
            &measured.sites,
        );
        report(
            &mut failures,
            "#[allow] budget",
            &compare(&baseline.allow_budgets(), &allows),
            &BTreeMap::new(),
        );
    }

    if failures.is_empty() {
        let total: usize = measured.counts.values().sum();
        println!("lint gate: {total} budgeted violations, all within budget");
        return Ok(());
    }
    for f in &failures {
        eprintln!("lint gate: {f}");
    }
    bail!(
        "{} quality-gate failure(s). Fix the code, or — if a budget really must \
         grow — edit {} deliberately so the increase shows up in review.",
        failures.len(),
        path.display()
    )
}

/// Judge one or more cargo-mutants output directories.
///
/// Two checks, in this order, because the second is meaningless without the
/// first: is this sweep believable at all, and then is it within budget.
fn mutants_gate(dirs: &[PathBuf], write: bool) -> Result<()> {
    if dirs.is_empty() {
        bail!("give at least one cargo-mutants output directory, e.g. mutants.out");
    }
    let sweeps: Vec<mutants::Sweep> = dirs
        .iter()
        .map(|d| mutants::read(&d.join("outcomes.json")))
        .collect::<Result<_>>()?;
    let sweep = mutants::merge(sweeps);

    for report in sweep.packages.values() {
        println!(
            "{:<24} {:>5} caught {:>5} missed {:>5} timeout {:>5} unviable",
            report.package, report.caught, report.missed, report.timeout, report.unviable
        );
    }

    let mut failures: Vec<String> = mutants::implausible(&sweep);
    if !failures.is_empty() {
        // Do not compare counts. A fabricated 100% would sail through any
        // budget, and recording one as a budget would enshrine it.
        for f in &failures {
            eprintln!("mutants gate: implausible result: {f}");
        }
        bail!(
            "{} package(s) reported results that cannot be true. Fix the sweep, not the budget.",
            failures.len()
        );
    }

    let path = baseline_path();
    let baseline = Baseline::load(&path)?;
    let missed = mutants::missed_counts(&sweep);

    if write {
        let next = baseline.with_mutants(&missed);
        write_baseline(&path, &next)?;
        println!(
            "wrote {} — {} surviving mutants across {} crates",
            path.display(),
            missed.values().sum::<usize>(),
            next.mutants.len()
        );
        return Ok(());
    }

    // Only crates this sweep actually covered are judged. A sharded or scoped
    // run legitimately says nothing about the rest, and treating silence as
    // zero would report every uncovered crate's budget as stale.
    let budgets: BTreeMap<String, usize> = baseline
        .mutant_budgets()
        .into_iter()
        .filter(|(krate, _)| sweep.packages.contains_key(krate))
        .collect();
    report(
        &mut failures,
        "mutants",
        &compare(&budgets, &missed),
        &BTreeMap::new(),
    );

    if failures.is_empty() {
        println!(
            "mutants gate: {} surviving mutants across {} crates, all within budget",
            missed.values().sum::<usize>(),
            sweep.packages.len()
        );
        return Ok(());
    }
    for f in &failures {
        eprintln!("mutants gate: {f}");
    }
    bail!(
        "{} quality-gate failure(s). A surviving mutant is either killed by a new \
         assertion or recorded, with a reason, in the Known-exceptions register in \
         docs/testing.md.",
        failures.len()
    )
}

/// Turn findings into failure lines, naming a few offending sites for the
/// regressions so the message is actionable without a second clippy run.
fn report(
    failures: &mut Vec<String>,
    label: &str,
    findings: &[Finding],
    sites: &BTreeMap<String, Vec<String>>,
) {
    for finding in findings {
        let mut line = format!("{label}: {}", finding.render());
        if finding.is_regression()
            && let Finding::Regressed { what, .. } = finding
            && let Some(where_) = sites.get(what)
        {
            let shown: Vec<&str> = where_.iter().take(5).map(String::as_str).collect();
            line.push_str(&format!("\n  at {}", shown.join(", ")));
            if where_.len() > shown.len() {
                line.push_str(&format!(" (+{} more)", where_.len() - shown.len()));
            }
        }
        failures.push(line);
    }
}

/// Rewrite the baseline from what was just measured.
///
/// This is `mise run baseline`, and it is allowed to move a count in either
/// direction — it is a deliberate, reviewable edit to a checked-in file. What
/// makes the ratchet a ratchet is that CI never runs it.
///
/// Only the measured tables are regenerated. `[meta]` is hand-written policy
/// (which lints are ratcheted, and why each one earns its place) and is carried
/// through verbatim apart from the toolchain stamp, because a TOML serializer
/// would drop every comment in it and the reasons are the valuable part.
fn rewrite(
    path: &Path,
    baseline: &Baseline,
    counts: &BTreeMap<String, usize>,
    allows: &BTreeMap<String, usize>,
) -> Result<()> {
    let rustc = lint::rustc_version()?;
    let next = baseline.rewritten(&rustc, counts, allows);
    write_baseline(path, &next)?;

    let before: usize = baseline.lints.iter().map(|b| b.count).sum();
    let after: usize = next.lints.iter().map(|b| b.count).sum();
    println!(
        "wrote {} — {before} -> {after} budgeted violations across {} rows",
        path.display(),
        next.lints.len()
    );
    Ok(())
}

/// Just the measured half of the file, so the hand-written `[meta]` block above
/// it can be preserved as text.
///
/// Every field is skipped when empty, and that is load-bearing rather than
/// tidiness. TOML serializes an empty `Vec` as a bare `key = []`, and a bare
/// key emitted before any table header belongs to whatever table precedes it —
/// which here is the hand-written `[meta]`. `header_of` then preserves it as
/// part of the header, so the next write appends another, and the file
/// accumulates one `mutants = []` per run until it fails to parse on a
/// duplicate key. With the field skipped, a non-empty `Vec<struct>` can only
/// ever emit `[[name]]` sections, so the body cannot contain a bare key at all.
#[derive(serde::Serialize)]
struct Tables {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lints: Vec<baseline::Budget>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allows: Vec<baseline::AllowBudget>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mutants: Vec<baseline::MutantBudget>,
}

/// Write a baseline, preserving the hand-written header and restamping the
/// toolchain. Shared by both gates, so neither can erase the other's tables.
fn write_baseline(path: &Path, next: &Baseline) -> Result<()> {
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let header = restamp_rustc(&header_of(&old), &next.meta.rustc);
    let body = toml::to_string_pretty(&Tables {
        lints: next.lints.clone(),
        allows: next.allows.clone(),
        mutants: next.mutants.clone(),
    })
    .context("serialize the quality baseline")?;
    let text = format!("{header}\n{body}");

    // Read back what we are about to write. A gate that corrupts its own
    // baseline fails every later run for a reason that has nothing to do with
    // the code under test, and the corruption is invisible until then.
    toml::from_str::<Baseline>(&text).with_context(|| {
        format!(
            "refusing to write {}: the generated file does not parse as a baseline",
            path.display()
        )
    })?;

    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

/// Everything above the first measured table: the file's explanatory comments
/// and the whole `[meta]` block, comments included.
fn header_of(text: &str) -> String {
    text.lines()
        .take_while(|l| !l.starts_with("[["))
        // A measured key at column zero is a previous write leaking into the
        // header. Drop it rather than carry it forward: keeping it is what let
        // one stray `mutants = []` become two and then a parse error.
        .filter(|l| !MEASURED_KEYS.iter().any(|k| l.starts_with(k)))
        .map(|l| format!("{l}\n"))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// Table names owned by the writer. Anything starting with one of these at
/// column zero is generated, never hand-written.
const MEASURED_KEYS: [&str; 3] = ["lints =", "allows =", "mutants ="];

/// Update the `rustc = "..."` line inside a preserved header.
///
/// The stamp has to move with the measurement — a baseline that claims an older
/// toolchain than the one it was taken on would fail the gate on the very next
/// run, for a reason that has nothing to do with the code.
fn restamp_rustc(header: &str, rustc: &str) -> String {
    header
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("rustc =") {
                format!("rustc = \"{rustc}\"")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "# why this file exists\n\n[meta]\nrustc = \"1.0.0\"\nratcheted = [\n    # because\n    \"clippy::x\",\n]\n";

    #[test]
    fn the_whole_hand_written_block_survives_a_rewrite_comments_included() {
        let text = format!("{HEADER}\n[[lints]]\ncrate = \"a\"\n");
        let header = header_of(&text);
        assert!(header.contains("# why this file exists"));
        assert!(
            header.contains("# because"),
            "comments inside [meta] are the valuable part"
        );
        assert!(
            !header.contains("[[lints]]"),
            "measured tables are regenerated, not preserved"
        );
    }

    #[test]
    fn a_measured_key_leaked_into_the_header_is_not_carried_forward() {
        // The exact corruption: an empty table serialized as a bare key lands
        // inside [meta], and preserving it appends another on the next write.
        let text = format!("{HEADER}mutants = []\n\n[[lints]]\ncrate = \"a\"\n");
        assert!(!header_of(&text).contains("mutants ="));
        assert!(
            header_of(&text).contains("# because"),
            "real header survives"
        );
    }

    #[test]
    fn a_file_that_is_only_measured_tables_yields_no_header() {
        assert_eq!(header_of("[[lints]]\ncrate = \"a\"\n"), "");
    }

    #[test]
    fn the_toolchain_stamp_moves_with_the_measurement() {
        let out = restamp_rustc(HEADER, "9.9.9");
        assert!(out.contains("rustc = \"9.9.9\""));
        assert!(!out.contains("1.0.0"));
        assert!(
            out.contains("# because"),
            "restamping must not touch anything else"
        );
    }

    #[test]
    fn a_regression_names_the_sites_that_caused_it() {
        let mut failures = Vec::new();
        let sites: BTreeMap<String, Vec<String>> = [(
            "a/clippy::x".to_owned(),
            vec!["a/src/one.rs:1".to_owned(), "a/src/two.rs:2".to_owned()],
        )]
        .into_iter()
        .collect();
        report(
            &mut failures,
            "lint",
            &[Finding::Regressed {
                what: "a/clippy::x".to_owned(),
                budget: 0,
                observed: 2,
            }],
            &sites,
        );
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].contains("a/src/one.rs:1, a/src/two.rs:2"),
            "{}",
            failures[0]
        );
    }

    #[test]
    fn a_long_site_list_is_truncated_with_a_count_of_the_rest() {
        let mut failures = Vec::new();
        let all: Vec<String> = (0..9).map(|i| format!("a/src/f.rs:{i}")).collect();
        let sites: BTreeMap<String, Vec<String>> =
            [("a/clippy::x".to_owned(), all)].into_iter().collect();
        report(
            &mut failures,
            "lint",
            &[Finding::Regressed {
                what: "a/clippy::x".to_owned(),
                budget: 0,
                observed: 9,
            }],
            &sites,
        );
        assert!(failures[0].contains("(+4 more)"), "{}", failures[0]);
    }

    #[test]
    fn a_stale_budget_reports_without_sites() {
        let mut failures = Vec::new();
        report(
            &mut failures,
            "lint",
            &[Finding::Stale {
                what: "a/clippy::x".to_owned(),
                budget: 3,
                observed: 1,
            }],
            &BTreeMap::new(),
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("stale"), "{}", failures[0]);
        assert!(!failures[0].contains("\n  at "));
    }
}
