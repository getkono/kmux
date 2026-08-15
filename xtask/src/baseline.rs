//! The quality baseline: a checked-in budget that may only shrink.
//!
//! The problem this solves is that kmux has thousands of violations of lints it
//! wants to hold, spread over fifteen crates. Turning one on outright breaks the
//! tree; leaving it off means the count grows silently. A ratchet does neither:
//! the current count is recorded per crate, CI fails when a count goes **up**,
//! and — just as important — CI also fails when a count goes **down** without
//! the baseline being updated. A budget nobody tightens is a budget that stops
//! describing the code, and a stale entry is exactly the room a future
//! regression slips into unnoticed.
//!
//! Tightening is automatic (`mise run baseline` rewrites the file from what was
//! measured); loosening is always a manual edit to a checked-in file, visible in
//! review. That asymmetry is the whole design.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The parsed `quality-baseline.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub meta: Meta,
    /// One row per (crate, lint). Rows are sorted on write so a diff shows only
    /// what actually changed.
    #[serde(default)]
    pub lints: Vec<Budget>,
    /// One row per crate: how many `#[allow]` attributes it still carries.
    #[serde(default)]
    pub allows: Vec<AllowBudget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// The toolchain the counts were measured on. A compiler upgrade changes
    /// what lints fire, so it must not be able to masquerade as a regression
    /// (or, worse, silently absorb one).
    pub rustc: String,
    /// The lints the gate injects. Anything measured at zero belongs in
    /// `[workspace.lints]` instead, where the compiler holds it for free.
    pub ratcheted: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Budget {
    #[serde(rename = "crate")]
    pub krate: String,
    pub lint: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AllowBudget {
    #[serde(rename = "crate")]
    pub krate: String,
    pub count: usize,
}

/// What the gate found wrong. Every variant is a hard failure: a ratchet with a
/// "warning" tier is a ratchet that slips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// More violations than budgeted. The regression case.
    Regressed {
        what: String,
        budget: usize,
        observed: usize,
    },
    /// Fewer violations than budgeted — the code improved but the budget still
    /// claims the old number, so it no longer describes anything.
    Stale {
        what: String,
        budget: usize,
        observed: usize,
    },
}

impl Finding {
    /// The one-line form printed by the gate.
    pub fn render(&self) -> String {
        match self {
            Self::Regressed {
                what,
                budget,
                observed,
            } => format!(
                "{what}: {observed} violations, budget is {budget} (+{})",
                observed - budget
            ),
            Self::Stale {
                what,
                budget,
                observed,
            } => format!(
                "{what}: {observed} violations, budget is {budget} — the budget is stale, tighten it"
            ),
        }
    }

    /// True for the "code got worse" half. Used only to word the summary; both
    /// halves fail.
    pub fn is_regression(&self) -> bool {
        matches!(self, Self::Regressed { .. })
    }
}

/// Compare measured counts against budgets.
///
/// `observed` and `budgets` are both keyed by whatever identifies a row —
/// `crate/lint` for lints, `crate` for allows — so this one function serves
/// both tables. A key present on one side and absent on the other is treated as
/// a count of zero on that side, which gives the right verdict without a
/// special case: a brand-new violation reads as `Regressed` from 0, and a
/// budget whose violations are all gone reads as `Stale` to 0.
pub fn compare(
    budgets: &BTreeMap<String, usize>,
    observed: &BTreeMap<String, usize>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let keys: std::collections::BTreeSet<&String> = budgets.keys().chain(observed.keys()).collect();
    for key in keys {
        let budget = budgets.get(key).copied().unwrap_or(0);
        let count = observed.get(key).copied().unwrap_or(0);
        if count > budget {
            findings.push(Finding::Regressed {
                what: key.clone(),
                budget,
                observed: count,
            });
        } else if count < budget {
            findings.push(Finding::Stale {
                what: key.clone(),
                budget,
                observed: count,
            });
        }
    }
    findings
}

impl Baseline {
    /// Read the baseline from disk.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the quality baseline at {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("parse the quality baseline at {}", path.display()))
    }

    /// Lint budgets keyed as `crate/lint`, the form [`compare`] wants.
    pub fn lint_budgets(&self) -> BTreeMap<String, usize> {
        self.lints
            .iter()
            .map(|b| (format!("{}/{}", b.krate, b.lint), b.count))
            .collect()
    }

    /// Suppression budgets keyed by crate.
    pub fn allow_budgets(&self) -> BTreeMap<String, usize> {
        self.allows
            .iter()
            .map(|b| (b.krate.clone(), b.count))
            .collect()
    }

    /// Rebuild from measured counts, keeping `meta` as-is apart from the
    /// toolchain stamp. Rows are sorted and zero counts dropped, so the file
    /// stays a description of what is left rather than a log of what was.
    #[must_use]
    pub fn rewritten(
        &self,
        rustc: &str,
        lints: &BTreeMap<String, usize>,
        allows: &BTreeMap<String, usize>,
    ) -> Self {
        let mut rows: Vec<Budget> = lints
            .iter()
            .filter(|&(_, &n)| n > 0)
            .filter_map(|(key, &count)| {
                let (krate, lint) = key.split_once('/')?;
                Some(Budget {
                    krate: krate.to_owned(),
                    lint: lint.to_owned(),
                    count,
                })
            })
            .collect();
        rows.sort();
        let mut allow_rows: Vec<AllowBudget> = allows
            .iter()
            .filter(|&(_, &n)| n > 0)
            .map(|(krate, &count)| AllowBudget {
                krate: krate.clone(),
                count,
            })
            .collect();
        allow_rows.sort();
        Self {
            meta: Meta {
                rustc: rustc.to_owned(),
                ratcheted: self.meta.ratcheted.clone(),
            },
            lints: rows,
            allows: allow_rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, usize)]) -> BTreeMap<String, usize> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    }

    #[test]
    fn a_count_equal_to_its_budget_is_silent() {
        let f = compare(&counts(&[("a/l", 3)]), &counts(&[("a/l", 3)]));
        assert_eq!(f, vec![]);
    }

    #[test]
    fn a_count_over_its_budget_regresses() {
        let f = compare(&counts(&[("a/l", 3)]), &counts(&[("a/l", 5)]));
        assert_eq!(
            f,
            vec![Finding::Regressed {
                what: "a/l".to_owned(),
                budget: 3,
                observed: 5
            }]
        );
        assert!(f[0].is_regression());
        assert_eq!(f[0].render(), "a/l: 5 violations, budget is 3 (+2)");
    }

    #[test]
    fn a_count_under_its_budget_is_stale_not_a_free_pass() {
        let f = compare(&counts(&[("a/l", 3)]), &counts(&[("a/l", 1)]));
        assert_eq!(
            f,
            vec![Finding::Stale {
                what: "a/l".to_owned(),
                budget: 3,
                observed: 1
            }]
        );
        assert!(!f[0].is_regression());
    }

    #[test]
    fn a_violation_with_no_budget_row_regresses_from_zero() {
        let f = compare(&counts(&[]), &counts(&[("new/l", 1)]));
        assert_eq!(
            f,
            vec![Finding::Regressed {
                what: "new/l".to_owned(),
                budget: 0,
                observed: 1
            }]
        );
    }

    #[test]
    fn a_budget_row_with_no_violations_left_is_stale() {
        let f = compare(&counts(&[("gone/l", 4)]), &counts(&[]));
        assert_eq!(
            f,
            vec![Finding::Stale {
                what: "gone/l".to_owned(),
                budget: 4,
                observed: 0
            }]
        );
    }

    #[test]
    fn findings_cover_every_key_from_both_sides() {
        let f = compare(
            &counts(&[("a/l", 1), ("b/l", 9)]),
            &counts(&[("b/l", 9), ("c/l", 2)]),
        );
        let what: Vec<&str> = f
            .iter()
            .map(|f| match f {
                Finding::Regressed { what, .. } | Finding::Stale { what, .. } => what.as_str(),
            })
            .collect();
        // b/l matches its budget and is silent; a/l and c/l are each wrong in
        // one direction, and both are reported.
        assert_eq!(what, vec!["a/l", "c/l"]);
    }

    fn sample() -> Baseline {
        Baseline {
            meta: Meta {
                rustc: "1.0.0".to_owned(),
                ratcheted: vec!["clippy::x".to_owned()],
            },
            lints: vec![Budget {
                krate: "a".to_owned(),
                lint: "clippy::x".to_owned(),
                count: 7,
            }],
            allows: vec![AllowBudget {
                krate: "a".to_owned(),
                count: 2,
            }],
        }
    }

    #[test]
    fn rewriting_tightens_counts_and_restamps_the_toolchain() {
        let out = sample().rewritten(
            "1.2.3",
            &counts(&[("a/clippy::x", 4)]),
            &counts(&[("a", 1)]),
        );
        assert_eq!(out.meta.rustc, "1.2.3");
        assert_eq!(out.lints[0].count, 4);
        assert_eq!(out.allows[0].count, 1);
        // The ratcheted-lint list is policy, not measurement, so it survives.
        assert_eq!(out.meta.ratcheted, vec!["clippy::x".to_owned()]);
    }

    #[test]
    fn rewriting_drops_rows_that_reached_zero() {
        let out = sample().rewritten(
            "1.2.3",
            &counts(&[("a/clippy::x", 0)]),
            &counts(&[("a", 0)]),
        );
        assert_eq!(out.lints, vec![]);
        assert_eq!(out.allows, vec![]);
    }

    #[test]
    fn rewriting_sorts_rows_so_a_diff_shows_only_real_change() {
        let out = sample().rewritten(
            "1.2.3",
            &counts(&[("z/clippy::x", 1), ("a/clippy::y", 1), ("a/clippy::x", 1)]),
            &counts(&[]),
        );
        let keys: Vec<String> = out
            .lints
            .iter()
            .map(|b| format!("{}/{}", b.krate, b.lint))
            .collect();
        assert_eq!(keys, vec!["a/clippy::x", "a/clippy::y", "z/clippy::x"]);
    }

    #[test]
    fn a_baseline_round_trips_through_toml() {
        let text = toml::to_string_pretty(&sample()).expect("serialize");
        let back: Baseline = toml::from_str(&text).expect("deserialize");
        assert_eq!(back, sample());
    }

    #[test]
    fn budgets_are_keyed_the_way_compare_expects() {
        let b = sample();
        assert_eq!(b.lint_budgets(), counts(&[("a/clippy::x", 7)]));
        assert_eq!(b.allow_budgets(), counts(&[("a", 2)]));
    }
}
