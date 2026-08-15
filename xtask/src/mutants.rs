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
//! real. The tell is timing. A caught mutant runs the whole test binary, so it
//! takes roughly as long as the baseline run; a mutant "caught" by a target
//! that does not exist takes no time at all. That ratio is self-calibrating —
//! no absolute threshold to tune per crate — and it makes the exact bug that
//! produced the fabricated score impossible to reintroduce quietly.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// A caught mutant whose test phase finished faster than this, in seconds, did
/// not run a test suite. Backstop for when the baseline duration is unavailable.
const IMPLAUSIBLY_FAST_SECS: f64 = 1.0;

/// …or faster than this fraction of the sweep's own baseline test phase. A
/// caught mutant runs the same test binary the baseline did and fails somewhere
/// inside it, so anything under a fifth of the baseline means the binary was
/// never reached.
const IMPLAUSIBLE_FRACTION_OF_BASELINE: f64 = 0.2;

#[derive(Debug, Deserialize)]
struct Outcomes {
    outcomes: Vec<Outcome>,
}

#[derive(Debug, Deserialize)]
struct Outcome {
    scenario: Scenario,
    summary: String,
    #[serde(default)]
    phase_results: Vec<PhaseResult>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Scenario {
    /// A scenario with no payload. In practice `"Baseline"` — the unmutated
    /// build, run once per sweep to prove the suite passes before anything is
    /// mutated. Kept as a string rather than a unit so an unrecognised scalar
    /// scenario from a future cargo-mutants is skipped rather than mistaken for
    /// the baseline.
    Named(String),
    Mutant {
        #[serde(rename = "Mutant")]
        mutant: MutantScenario,
    },
}

#[derive(Debug, Deserialize)]
struct MutantScenario {
    package: String,
}

#[derive(Debug, Deserialize)]
struct PhaseResult {
    phase: String,
    duration: f64,
}

/// What one package's mutants did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PackageReport {
    pub package: String,
    pub caught: usize,
    pub missed: usize,
    pub timeout: usize,
    pub unviable: usize,
    /// The longest test phase among this package's caught mutants. Zero when
    /// none were caught.
    pub slowest_caught_test: f64,
}

impl PackageReport {
    /// Mutants that actually got a verdict. Unviable ones did not compile, so
    /// they say nothing about the tests either way.
    pub fn scored(&self) -> usize {
        self.caught + self.missed + self.timeout
    }
}

/// One sweep: the per-package tallies plus the baseline that calibrates them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Sweep {
    pub packages: BTreeMap<String, PackageReport>,
    /// The baseline scenario's test-phase duration, if the sweep ran one.
    pub baseline_test: Option<f64>,
}

/// Parse one `outcomes.json`.
pub fn read(path: &Path) -> Result<Sweep> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read the mutation outcomes at {}", path.display()))?;
    parse(&text).with_context(|| format!("parse the mutation outcomes at {}", path.display()))
}

/// Parse an `outcomes.json` document.
pub fn parse(text: &str) -> Result<Sweep> {
    let doc: Outcomes = serde_json::from_str(text).context("decode outcomes.json")?;
    let mut sweep = Sweep::default();
    for outcome in doc.outcomes {
        let test_secs = outcome
            .phase_results
            .iter()
            .find(|p| p.phase == "Test")
            .map(|p| p.duration);
        let package = match &outcome.scenario {
            Scenario::Named(name) if name == "Baseline" => {
                sweep.baseline_test = test_secs;
                continue;
            }
            Scenario::Named(_) => continue,
            Scenario::Mutant { mutant } => mutant.package.clone(),
        };
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
                entry.slowest_caught_test = entry.slowest_caught_test.max(test_secs.unwrap_or(0.0));
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
/// its own output directory and its own baseline.
///
/// The merged baseline is the **smallest** of the inputs, because it is used as
/// a lower bound on how long a real test run takes: taking the largest would
/// let a slow group's baseline excuse a fast group's fabricated catches.
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
            entry.slowest_caught_test = entry.slowest_caught_test.max(report.slowest_caught_test);
        }
        out.baseline_test = match (out.baseline_test, sweep.baseline_test) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    out
}

/// Packages whose results cannot be believed, with the reason.
///
/// The condition is a perfect score reached implausibly fast. A perfect score
/// alone is not suspicious — a small, well-tested module can earn one — and
/// speed alone is not either. Together, on a package where every single mutant
/// was caught, they are the signature of a test command that failed before it
/// ran anything.
pub fn implausible(sweep: &Sweep) -> Vec<String> {
    let mut found = Vec::new();
    for report in sweep.packages.values() {
        if report.missed > 0 || report.caught == 0 {
            continue;
        }
        // The ratio is the real test, because it calibrates itself to the
        // crate. The absolute threshold is only a fallback for when there is no
        // usable baseline to calibrate against — including a baseline that is
        // itself under a second, which is too short to divide by meaningfully.
        let calibrator = sweep.baseline_test.filter(|b| *b >= IMPLAUSIBLY_FAST_SECS);
        let flagged = match calibrator {
            Some(b) => report.slowest_caught_test < b * IMPLAUSIBLE_FRACTION_OF_BASELINE,
            None => report.slowest_caught_test < IMPLAUSIBLY_FAST_SECS,
        };
        if !flagged {
            continue;
        }
        let against = calibrator.map_or_else(
            || String::from("no usable baseline in this sweep"),
            |b| format!("baseline test phase took {b:.2}s"),
        );
        found.push(format!(
            "{}: {} caught, 0 missed, but the slowest catch took {:.2}s ({against}). \
             A caught mutant runs the whole test binary; this one never reached it. \
             Check that the crate's target shape matches its .cargo/mutants*.toml \
             config — `--lib` hard-errors on a bin-only package, and cargo-mutants \
             reads that error as a catch.",
            report.package, report.caught, report.slowest_caught_test
        ));
    }
    found
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

    /// Build an outcomes.json document from `(package, summary, test_secs)`
    /// triples, plus an optional baseline test duration.
    fn doc(baseline: Option<f64>, mutants: &[(&str, &str, f64)]) -> String {
        let mut outcomes: Vec<String> = Vec::new();
        if let Some(secs) = baseline {
            outcomes.push(format!(
                r#"{{"scenario":"Baseline","summary":"Success","phase_results":[{{"phase":"Test","duration":{secs},"process_status":"Success"}}]}}"#
            ));
        }
        for (pkg, summary, secs) in mutants {
            outcomes.push(format!(
                r#"{{"scenario":{{"Mutant":{{"package":"{pkg}","name":"n","file":"f"}}}},"summary":"{summary}","phase_results":[{{"phase":"Build","duration":1.0,"process_status":"Success"}},{{"phase":"Test","duration":{secs},"process_status":"Success"}}]}}"#
            ));
        }
        format!(r#"{{"outcomes":[{}]}}"#, outcomes.join(","))
    }

    #[test]
    fn outcomes_are_tallied_per_package() {
        let sweep = parse(&doc(
            Some(4.0),
            &[
                ("a", "CaughtMutant", 3.0),
                ("a", "MissedMutant", 3.0),
                ("a", "Unviable", 0.0),
                ("b", "Timeout", 30.0),
            ],
        ))
        .expect("parse");
        assert_eq!(sweep.packages["a"].caught, 1);
        assert_eq!(sweep.packages["a"].missed, 1);
        assert_eq!(sweep.packages["a"].unviable, 1);
        assert_eq!(
            sweep.packages["a"].scored(),
            2,
            "unviable mutants are not a verdict"
        );
        assert_eq!(sweep.packages["b"].timeout, 1);
        assert_eq!(sweep.baseline_test, Some(4.0));
    }

    #[test]
    fn the_june_bug_is_reported_as_implausible() {
        // The exact shape: every mutant "caught", each in about the tenth of a
        // second `cargo test --lib` takes to fail with "no library targets".
        let sweep = parse(&doc(
            Some(9.5),
            &[
                ("kmuxd", "CaughtMutant", 0.10),
                ("kmuxd", "CaughtMutant", 0.11),
                ("kmuxd", "CaughtMutant", 0.09),
            ],
        ))
        .expect("parse");
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

    #[test]
    fn a_real_perfect_score_is_believed() {
        let sweep = parse(&doc(
            Some(4.0),
            &[("a", "CaughtMutant", 3.9), ("a", "CaughtMutant", 4.2)],
        ))
        .expect("parse");
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn a_package_with_survivors_is_never_flagged() {
        // Something survived, so the suite demonstrably ran. Fast catches here
        // mean fast tests, not absent ones.
        let sweep = parse(&doc(
            Some(9.5),
            &[("a", "CaughtMutant", 0.10), ("a", "MissedMutant", 0.10)],
        ))
        .expect("parse");
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn a_baseline_exonerates_catches_that_are_fast_in_proportion_to_it() {
        // A genuinely quick suite: the catches take nearly as long as the
        // baseline, which is what a real run looks like at any scale.
        let sweep = parse(&doc(Some(1.5), &[("a", "CaughtMutant", 1.4)])).expect("parse");
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn with_no_baseline_to_calibrate_against_the_absolute_backstop_applies() {
        let sweep = parse(&doc(None, &[("a", "CaughtMutant", 0.30)])).expect("parse");
        assert_eq!(implausible(&sweep).len(), 1);
    }

    #[test]
    fn a_baseline_too_short_to_divide_by_is_not_used_as_a_calibrator() {
        // A sub-second baseline would "exonerate" a 0.1s catch at any ratio, so
        // it is discarded and the absolute rule applies instead.
        let sweep = parse(&doc(Some(0.4), &[("a", "CaughtMutant", 0.35)])).expect("parse");
        assert_eq!(implausible(&sweep).len(), 1);
        assert!(implausible(&sweep)[0].contains("no usable baseline"));
    }

    #[test]
    fn a_package_where_nothing_was_caught_is_not_flagged() {
        let sweep = parse(&doc(Some(9.5), &[("a", "MissedMutant", 0.1)])).expect("parse");
        assert_eq!(implausible(&sweep), Vec::<String>::new());
    }

    #[test]
    fn merging_sums_packages_and_keeps_the_strictest_baseline() {
        let a = parse(&doc(Some(9.0), &[("p", "CaughtMutant", 8.0)])).expect("parse");
        let b = parse(&doc(
            Some(2.0),
            &[("p", "MissedMutant", 0.0), ("q", "CaughtMutant", 1.9)],
        ))
        .expect("parse");
        let merged = merge([a, b]);
        assert_eq!(merged.packages["p"].caught, 1);
        assert_eq!(merged.packages["p"].missed, 1);
        assert_eq!(merged.packages.len(), 2);
        assert_eq!(
            merged.baseline_test,
            Some(2.0),
            "the smallest baseline is the honest lower bound on a real test run"
        );
    }

    #[test]
    fn merging_keeps_the_slowest_catch_seen_anywhere() {
        let a = parse(&doc(None, &[("p", "CaughtMutant", 0.1)])).expect("parse");
        let b = parse(&doc(None, &[("p", "CaughtMutant", 7.0)])).expect("parse");
        assert_eq!(merge([a, b]).packages["p"].slowest_caught_test, 7.0);
    }

    #[test]
    fn missed_counts_include_packages_that_missed_nothing() {
        let sweep = parse(&doc(
            Some(4.0),
            &[("a", "MissedMutant", 1.0), ("b", "CaughtMutant", 3.0)],
        ))
        .expect("parse");
        // `b` has to appear with 0, or a budget row for it would read as stale
        // and a later regression in it would have nothing to be measured against.
        assert_eq!(missed_counts(&sweep)["a"], 1);
        assert_eq!(missed_counts(&sweep)["b"], 0);
    }

    #[test]
    fn an_unrecognised_scalar_scenario_is_skipped_not_taken_for_the_baseline() {
        let text = r#"{"outcomes":[{"scenario":"SomethingNew","summary":"Success","phase_results":[{"phase":"Test","duration":99.0,"process_status":"Success"}]}]}"#;
        let sweep = parse(text).expect("parse");
        assert_eq!(sweep.baseline_test, None);
        assert!(sweep.packages.is_empty());
    }

    #[test]
    fn a_sweep_with_no_test_phase_does_not_panic() {
        let text = r#"{"outcomes":[{"scenario":{"Mutant":{"package":"a"}},"summary":"CaughtMutant","phase_results":[]}]}"#;
        let sweep = parse(text).expect("parse");
        assert_eq!(sweep.packages["a"].slowest_caught_test, 0.0);
        assert_eq!(
            implausible(&sweep).len(),
            1,
            "no timing at all is not evidence of a real run"
        );
    }
}
