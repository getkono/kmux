# Quality gates

This document is **normative**. It describes what the tree is checked against,
where each check lives, and how to change a rule. [docs/testing.md](testing.md)
is its companion: that one says how to write tests, this one says what enforces
them.

The organising idea is that a rule the compiler does not check is not a rule.
Before this document existed, `docs/crate-usage.md` R5 ("layering is a dependency
rule, not a convention") and `kmux-app/src/lib.rs`'s "Hard rule: nothing in this
crate may depend on a UI toolkit" were both true statements that nothing
executable enforced. Every rule below names the thing that fails when it is
broken.

## The three tiers

| Tier | Where it is declared | What happens on violation |
|---|---|---|
| **Hard** | `[workspace.lints]` in the root `Cargo.toml`, plus `clippy.toml` | The build fails. `mise run clippy` runs `-D warnings`, so every entry is fatal in the hooks and in CI. |
| **Ratcheted** | `quality-baseline.toml` `[meta].ratcheted` | `mise run lint-gate` fails if the count for a crate goes **up** — or **down** without the baseline being updated. |
| **Structural** | `xtask/tests/dependency_direction.rs` | An ordinary test failure, so it rides `mise run test`, the pre-push hook, and CI with no extra step. |

**Hard lints never live in the ratchet, and ratcheted lints never live in
`Cargo.toml`.** That is not style: because `mise run clippy` is `-D warnings`,
anything declared at `warn` in `[workspace.lints]` is already fatal, so
declaring a lint with violations there breaks the tree on the spot. Ratcheted
lints are injected by the gate instead, which also keeps `cargo build` and
rust-analyzer quiet about debt you are not currently paying down.

## The ratchet

```
mise run lint-gate       # check — this is what CI runs
mise run baseline        # re-measure and rewrite quality-baseline.toml
```

`quality-baseline.toml` records, per crate and per lint, how many violations
remain. The gate fails in **both** directions:

* **Count went up.** A regression. The failure names up to five offending
  `file:line` sites, so it is actionable without a second run.
* **Count went down, baseline unchanged.** The budget is stale. This is the half
  people find surprising and it is the half that makes the ratchet work: a
  budget of 40 against 12 real violations is 28 violations of headroom that a
  future change can grow into without anyone noticing. Run `mise run baseline`.

Three details carry most of the weight:

**`--force-warn`, not `-W`.** The gate injects each ratcheted lint as
`--force-warn`, which outranks `-D warnings` (so the lint stays a warning and
gets *counted* rather than failing the compile) **and** any in-source `#[allow]`
(so a budget cannot be paid down by suppressing instead of fixing). Verified:
adding `#[allow(clippy::unwrap_used)]` over a new `.unwrap()` still fails the
gate, and additionally trips the `[[allows]]` budget.

**The `[[allows]]` budget.** A separate per-crate count of `#[allow]`
attributes. `#[expect(…, reason = "…")]` is deliberately not counted: it expires
by itself once it stops applying (`unfulfilled_lint_expectations` is a hard
lint), which is why AGENTS.md requires it for every new suppression. `#[allow]`
never expires, so what is left of it is debt with a number.

**Refusing partial measurements.** cargo stops at the first crate that fails, so
one hard-lint error leaves everything downstream of it unlinted — and unlinted
reads as *zero violations*. Writing that as a baseline would record budgets that
are too loose for half the tree. `mise run baseline` therefore refuses to write
when the run did not finish, and `lint-gate` skips budget comparison entirely
when it sees hard-lint errors, so the real cause is not buried under a dozen
phantom "stale budget" lines. This failure mode is not hypothetical: it is
exactly how the June 2026 mutation baseline came to report a fabricated 100% for
three crates.

### Graduating a lint

When a ratcheted lint reaches zero everywhere, it stops needing a budget and
starts being free:

1. Delete it from `[meta].ratcheted` in `quality-baseline.toml`.
2. Add it to `[workspace.lints]` in the root `Cargo.toml`, with a comment
   saying what clearing it involved.
3. `mise run baseline` to drop the now-empty rows.

Two lines of real change. The migration from column two to column one is the
point of the whole exercise; the baseline is scaffolding, not architecture.

### Toolchain stamps

`[meta].rustc` records the toolchain the counts were measured on. A compiler
upgrade changes what fires, so without the stamp an upgrade could masquerade as
a regression — or, worse, absorb one in the same commit that changes code. The
gate fails on a mismatch and asks for a deliberate re-measure in a commit of its
own.

## Structural rules

`cargo test -p xtask` asserts five things about the dependency graph, read from
`cargo metadata`:

* No UI toolkit is reachable from `kmux`, `kmux-app`, `kmux-client`, or
  `kmux-protocol` — the failure prints the shortest offending path, e.g.
  `kmux -> kmux-app -> gtk4`, not just the fact.
* `kmux-protocol` depends on no internal crate.
* The internal graph is acyclic.
* `kmux-worker-protocol` never reaches a GUI frontend.
* Every workspace member is classified, so a sixteenth crate cannot silently
  escape the rules by being new.

Deliberately no `--filter-platform`: the platform-union graph is the strictest
reading and gives identical verdicts on the Linux and macOS runners.

`cargo run -p xtask -- deps-graph` prints the graph these read, for when one
fails and you want to see what it saw.

## Mutation score

Coverage is measured by mutation score, not line count. `mise run mutants`
dispatches by crate shape (`--lib`, `--bins`, `--bins --tests`) because
cargo-mutants passes `additional_cargo_test_args` through verbatim and `--lib`
hard-errors on a bin-only package — which it then reads as "mutant caught".
See [docs/testing.md](testing.md) for the methodology, the per-crate table, and
the register of intentionally-untested areas.

## Running everything

| Command | Covers |
|---|---|
| `mise run fmt-check` | formatting |
| `mise run clippy` | the hard tier |
| `mise run lint-gate` | the hard tier **and** the ratchet |
| `mise run test` | tests, including the structural assertions |
| `mise run build-no-gpu` | the lean, wgpu-free path |
| `mise run mutants` | mutation score |

`lint-gate` subsumes `clippy`, so CI runs the gate and not both. The pre-push
hook still runs `clippy`, which is the faster check and the one that catches the
common case.
