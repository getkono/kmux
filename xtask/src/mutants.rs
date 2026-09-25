//! Read a cargo-mutants sweep and decide whether to believe it.
//!
//! The second half of that sentence is the reason this module exists. On
//! 2026-06-14 the recorded sweep reported `kmuxd` 712 caught / 0 missed,
//! `kmux-gtk` 608/0 and `kmux` 15/0 — a perfect score for 24k lines that had
//! never been mutation-tested at all. cargo-mutants passes
//! `additional_cargo_test_args` through verbatim, the config said `--lib`, and
//! `cargo test --package=kmuxd --lib` hard-errors with "no library targets
//! found in package" in about a tenth of a second. cargo-mutants saw a non-zero
//! exit and recorded it, correctly by its own lights, as "the tests failed, so
//! the mutant was caught" — 1,320 times.
//!
//! A number that wrong is worse than no number: it was cited as evidence the
//! daemon was well covered. So the gate does not only compare counts against a
//! budget, it first asks whether the sweep it is reading could possibly be
//! real. The tell is in each mutant's log. A caught mutant ran the test binary
//! and some test in it failed, so its log shows the libtest harness starting —
//! `running N tests`. A mutant "caught" by a target that does not exist has a
//! log with cargo's error and no harness at all. That is evidence rather than
//! inference, and it makes the exact bug that produced the fabricated score
//! impossible to reintroduce quietly.
//!
//! An earlier version inferred it from timing — a catch faster than a fifth of
//! the sweep's baseline, or than one second without one — and would have
//! halted the gate on genuine perfect scores: a crate-group's baseline tests
//! every crate in the group while a mutant's test phase tests only its own, and
//! a small crate's whole suite runs well under a second.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Outcomes {
    outcomes: Vec<Outcome>,
}

#[derive(Debug, Deserialize)]
struct Outcome {
    scenario: Scenario,
    summary: String,
    /// The mutant's log, relative to the output directory.
    #[serde(default)]
    log_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Scenario {
    Mutant {
        #[serde(rename = "Mutant")]
        mutant: MutantScenario,
    },
    /// Anything else: in practice `"Baseline"`, the unmutated build run once
    /// per sweep, or a scenario a future cargo-mutants adds. None of them is a
    /// verdict on a package. Tried after `Mutant`, since it matches anything.
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, Deserialize)]
struct MutantScenario {
    package: String,
}

/// What one package's mutants did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PackageReport {
    /// Crate these mutants belong to.
    pub package: String,
    /// Mutants some test failed on. A timeout is counted separately.
    pub caught: usize,
    /// Mutants that survived: no assertion anywhere noticed the change.
    pub missed: usize,
    /// Mutants that made the suite hang. Counted as caught — the suite
    /// noticing is the point — but tracked apart because the cost differs.
    pub timeout: usize,
    /// Mutants that did not compile, so they say nothing either way.
    pub unviable: usize,
    /// Caught mutants whose log shows the test harness running — evidence the
    /// catch came from a test, not from a test command that never started.
    pub caught_ran_tests: usize,
}

impl PackageReport {
    /// Mutants that actually got a verdict. Unviable ones did not compile, so
    /// they say nothing about the tests either way.
    pub fn scored(&self) -> usize {
        self.caught + self.missed + self.timeout
    }
}

/// One sweep: the per-package tallies.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sweep {
    /// Per-crate tallies, keyed by crate name.
    pub packages: BTreeMap<String, PackageReport>,
}

/// Parse one `outcomes.json`, reading each caught mutant's log from beside it.
///
/// # Errors
/// If the file cannot be read or is not a valid outcomes document.
pub fn read(path: &Path) -> Result<Sweep> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the mutation outcomes at {}", path.display()))?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    parse(&text, |log| std::fs::read_to_string(dir.join(log)).ok())
        .with_context(|| format!("parse the mutation outcomes at {}", path.display()))
}

/// Whether a mutant's log shows the libtest harness starting: a line
/// `running N test` or `running N tests`, which every test binary prints before
/// running anything.
fn shows_test_harness(log: &str) -> bool {
    log.lines().any(|line| {
        line.strip_prefix("running ")
            .and_then(|rest| {
                rest.strip_suffix(" tests")
                    .or_else(|| rest.strip_suffix(" test"))
            })
            .is_some_and(|n| n.parse::<usize>().is_ok())
    })
}

/// Parse an `outcomes.json` document. `log` returns the text of a mutant's log
/// given its `log_path`, or `None` when it cannot be read.
///
/// # Errors
/// If the text is not valid JSON in cargo-mutants' outcomes shape.
pub fn parse(text: &str, log: impl Fn(&str) -> Option<String>) -> Result<Sweep> {
    let doc: Outcomes = serde_json::from_str(text).context("decode outcomes.json")?;
    let mut sweep = Sweep::default();
    for outcome in doc.outcomes {
        // Other scenarios — the unmutated `Baseline`, and anything a future
        // cargo-mutants adds — are not verdicts on a package.
        let Scenario::Mutant { mutant } = &outcome.scenario else {
            continue;
        };
        let package = mutant.package.clone();
        let entry = sweep
            .packages
            .entry(package.clone())
            .or_insert_with(|| PackageReport {
                package,
                ..PackageReport::default()
            });
        match outcome.summary.as_str() {
            "CaughtMutant" => {
                entry.caught += 1;
                let ran = outcome
                    .log_path
                    .as_deref()
                    .and_then(&log)
                    .is_some_and(|text| shows_test_harness(&text));
                entry.caught_ran_tests += usize::from(ran);
            }
            "MissedMutant" => entry.missed += 1,
            // A timeout is a catch: the mutant made the suite hang, which the
            // suite noticing is the point.
            "Timeout" => entry.timeout += 1,
            "Unviable" => entry.unviable += 1,
            _ => {}
        }
    }
    Ok(sweep)
}

/// Merge sweeps — the unscoped run makes one pass per crate-group, each with
/// its own output directory.
pub fn merge(sweeps: impl IntoIterator<Item = Sweep>) -> Sweep {
    let mut out = Sweep::default();
    for sweep in sweeps {
        for (name, report) in sweep.packages {
            let entry = out.packages.entry(name).or_insert_with(|| PackageReport {
                package: report.package.clone(),
                ..PackageReport::default()
            });
            entry.caught += report.caught;
            entry.missed += report.missed;
            entry.timeout += report.timeout;
            entry.unviable += report.unviable;
            entry.caught_ran_tests += report.caught_ran_tests;
        }
    }
    out
}

/// Packages whose results cannot be believed, with the reason.
///
/// The condition is a perfect score with no caught mutant whose log shows a
/// test running. A perfect score alone is not suspicious — a small, well-tested
/// crate can earn one — but a real catch always starts the test harness, so a
/// package where every mutant was caught and not one catch did is the signature
/// of a test command that failed before it ran anything. A package with a
/// survivor is never flagged: the suite demonstrably ran.
pub fn implausible(sweep: &Sweep) -> Vec<String> {
    sweep
        .packages
        .values()
        .filter(|r| r.missed == 0 && r.caught > 0 && r.caught_ran_tests == 0)
        .map(|r| {
            format!(
                "{}: {} caught, 0 missed, but no caught mutant's log shows a test \
                 running (`running N tests`). A caught mutant runs the test binary; \
                 these never reached it. Check that the crate's target shape matches \
                 its .cargo/mutants*.toml config — `--lib` hard-errors on a bin-only \
                 package, and cargo-mutants reads that error as a catch.",
                r.package, r.caught
            )
        })
        .collect()
}

/// Missed-mutant counts keyed by package, in the form the ratchet compares.
pub fn missed_counts(sweep: &Sweep) -> BTreeMap<String, usize> {
    sweep
        .packages
        .values()
        .map(|r| (r.package.clone(), r.missed))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a real catch's log holds after the test command: the harness
    /// starting, then a failure. Abridged from a real cargo-mutants 27 log.
    const RAN: &str = "*** cargo test --package=a\n\nrunning 181 tests\n\
                       test result: FAILED. 180 passed; 1 failed\n*** result: Failure(101)\n";
    /// The June shape: cargo refusing before any test binary starts.
    const NEVER_RAN: &str = "*** cargo test --package=kmuxd --lib\n\
                             error: no library targets found in package `kmuxd`\n\
                             *** result: Failure(101)\n";

    /// Parse a sweep of `(package, summary, log)` mutants, where `log` is the
    /// mutant's log text (`None`: cargo-mutants recorded no readable log).
    fn sweep_of(mutants: &[(&str, &str, Option<&str>)]) -> Sweep {
        let mut outcomes = vec![
            r#"{"scenario":"Baseline","summary":"Success","log_path":"log/baseline.log"}"#
                .to_string(),
        ];
        let mut logs = BTreeMap::new();
        for (i, (pkg, summary, log)) in mutants.iter().enumerate() {
            let path = format!("log/{i}.log");
            if let Some(text) = log {
                logs.insert(path.clone(), (*text).to_string());
            }
            outcomes.push(format!(
                r#"{{"scenario":{{"Mutant":{{"package":"{pkg}","name":"n","file":"f"}}}},"summary":"{summary}","log_path":"{path}"}}"#
            ));
        }
        let text = format!(r#"{{"outcomes":[{}]}}"#, outcomes.join(","));
        parse(&text, |path| logs.get(path).cloned()).expect("parse")
    }

    #[test]
    fn outcomes_are_tallied_per_package() {
        let sweep = sweep_of(&[
            ("a", "CaughtMutant", Some(RAN)),
            ("a", "MissedMutant", Some(RAN)),
            ("a", "Unviable", None),
            ("b", "Timeout", None),
        ]);
        assert_eq!(sweep.packages["a"].caught, 1);
        assert_eq!(sweep.packages["a"].caught_ran_tests, 1);
        assert_eq!(sweep.packages["a"].missed, 1);
        assert_eq!(sweep.packages["a"].unviable, 1);
        assert_eq!(
            sweep.packages["a"].scored(),
            2,
            "unviable mutants are not a verdict"
        );
        assert_eq!(sweep.packages["b"].timeout, 1);
        assert_eq!(sweep.packages.len(), 2, "the baseline is not a package");
    }

    #[test]
    fn the_harness_line_is_recognised_in_both_numbers() {
        assert!(shows_test_harness("running 1 test\n"));
        assert!(shows_test_harness("x\nrunning 12 tests\ny"));
        assert!(!shows_test_harness("running the build\n"));
        assert!(!shows_test_harness("error: no library targets found\n"));
    }

    #[test]
    fn the_june_bug_is_reported_as_implausible() {
        // The exact shape: every mutant "caught" by `cargo test --lib` failing
        // with "no library targets" before any test binary started.
        let sweep = sweep_of(&[
            ("kmuxd", "CaughtMutant", Some(NEVER_RAN)),
            ("kmuxd", "CaughtMutant", Some(NEVER_RAN)),
            ("kmuxd", "CaughtMutant", Some(NEVER_RAN)),
        ]);
        let found = implausible(&sweep);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("kmuxd: 3 caught, 0 missed"),
            "{}",
            found[0]
        );
        assert!(
            found[0].contains("target shape"),
            "the message must say what to check"
        );
    }

    /// A small crate whose whole suite runs in a third of a second — measured
    /// on a real crate: 0.33s catches against a 0.87s baseline, which the
    /// timing rule this replaced flagged. What makes it real is the harness.
    #[test]
    fn a_real_perfect_score_is_believed_however_fast() {
        let sweep = sweep_of(&[
            ("a", "CaughtMutant", Some(RAN)),
            ("a", "CaughtMutant", Some(RAN)),
        ]);
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn a_package_with_survivors_is_never_flagged() {
        // Something survived, so the suite demonstrably ran.
        let sweep = sweep_of(&[
            ("a", "CaughtMutant", Some(NEVER_RAN)),
            ("a", "MissedMutant", Some(RAN)),
        ]);
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    /// No readable log is no evidence of a real run.
    #[test]
    fn catches_without_a_log_are_not_believed() {
        let sweep = sweep_of(&[("a", "CaughtMutant", None)]);
        assert_eq!(implausible(&sweep).len(), 1);
    }

    #[test]
    fn one_catch_that_ran_the_suite_vouches_for_the_command() {
        let sweep = sweep_of(&[
            ("a", "CaughtMutant", None),
            ("a", "CaughtMutant", Some(RAN)),
        ]);
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn a_package_where_nothing_was_caught_is_not_flagged() {
        let sweep = sweep_of(&[("a", "MissedMutant", Some(RAN))]);
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn merging_sums_packages_and_their_evidence() {
        let a = sweep_of(&[("p", "CaughtMutant", Some(RAN))]);
        let b = sweep_of(&[
            ("p", "MissedMutant", Some(RAN)),
            ("q", "CaughtMutant", Some(NEVER_RAN)),
        ]);
        let merged = merge([a, b]);
        assert_eq!(merged.packages["p"].caught, 1);
        assert_eq!(merged.packages["p"].caught_ran_tests, 1);
        assert_eq!(merged.packages["p"].missed, 1);
        assert_eq!(merged.packages["q"].caught_ran_tests, 0);
        assert_eq!(merged.packages.len(), 2);
    }

    #[test]
    fn missed_counts_include_packages_that_missed_nothing() {
        let sweep = sweep_of(&[
            ("a", "MissedMutant", Some(RAN)),
            ("b", "CaughtMutant", Some(RAN)),
        ]);
        // `b` has to appear with 0, or a budget row for it would read as stale
        // and a later regression in it would have nothing to be measured against.
        assert_eq!(missed_counts(&sweep)["a"], 1);
        assert_eq!(missed_counts(&sweep)["b"], 0);
    }

    #[test]
    fn an_unrecognised_scalar_scenario_is_skipped() {
        let text = r#"{"outcomes":[{"scenario":"SomethingNew","summary":"Success"}]}"#;
        let sweep = parse(text, |_| None).expect("parse");
        assert!(sweep.packages.is_empty());
    }

    /// `read` resolves each log against the directory `outcomes.json` is in.
    #[test]
    fn read_finds_the_logs_beside_the_outcomes() {
        let dir = std::env::temp_dir().join(format!("xtask-mutants-read-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("log")).expect("mkdir");
        std::fs::write(dir.join("log/m.log"), RAN).expect("log");
        let outcomes = dir.join("outcomes.json");
        std::fs::write(
            &outcomes,
            r#"{"outcomes":[{"scenario":{"Mutant":{"package":"a"}},"summary":"CaughtMutant","log_path":"log/m.log"}]}"#,
        )
        .expect("outcomes");
        let ran = read(&outcomes).map(|s| s.packages["a"].caught_ran_tests);
        std::fs::remove_dir_all(&dir).expect("clean up");
        assert_eq!(ran.expect("read"), 1);
    }
}
