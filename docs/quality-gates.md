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
| **Supply chain** | `deny.toml`, plus `[package.metadata.cargo-machete]` per manifest | `mise run deps-audit` fails on a licence outside the allow-list, an unignored RUSTSEC advisory, or a declared dependency nothing uses. |

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

## Supply chain

`mise run deps-audit` is cargo-deny plus cargo-machete. Both were added because
nothing checked what they check:

* **Licences.** kmux ships `AGPL-3.0-only OR LicenseRef-Commercial`. A licence
  that is fine for the AGPL build can still be a problem for the other half, so
  "nobody has looked" is not a position a dual-licensed project can hold.
  `deny.toml` carries the allow-list; two weak-copyleft entries (MPL-2.0 for
  uniffi, LGPL for a wasi-only shim) are there with the reason attached.
* **Advisories.** A daemon speaking QUIC and TLS sits on rustls, ring and
  quinn, and had no RUSTSEC check at all. The first run found three live
  vulnerabilities — a remote memory exhaustion in quinn-proto from unbounded
  out-of-order stream reassembly, a reachable panic in rustls-webpki's CRL
  parsing, and unsoundness in `anyhow::Error::downcast_mut`. All three were a
  lockfile update away. Unmaintained crates are flagged at the strictest
  setting (`unmaintained = "all"`, the whole graph) and each one currently in
  the tree is listed in `deny.toml` with a reason, so it is a decision on the
  record rather than a category switched off.
* **Unused dependencies.** cargo-machete replaces the R4b grep in
  `docs/crate-usage.md`, which that document itself admitted "is a name check,
  not a compiler check" — it passed a dependency named only in a comment. Its
  first run found `kmux-ghostty` declared by `kmuxd` and used by nothing.

cargo-machete reads `use` statements, so a dependency used any other way — via
`#[serde(with = "…")]`, or purely for its build-script metadata — needs an
`ignored` entry in that manifest's `[package.metadata.cargo-machete]`, with a
comment saying why. An entry without one is the thing to be suspicious of.

## Documentation

`RUSTDOCFLAGS=-D warnings` on `mise run doc-check`. A rustdoc warning is a
broken intra-doc link or a malformed doc attribute: the docs say one thing and
render another, which is worse than saying nothing at all.

## Mutation score

Coverage is measured by mutation score, not line count. Two commands:

```
mise run mutants          # sweep, one pass per crate-group
mise run mutants-gate     # judge the sweep, then hold it against the baseline
```

`mise run mutants` dispatches by crate shape (`--lib`, `--bins`,
`--bins --tests`) because cargo-mutants passes `additional_cargo_test_args`
through verbatim and `--lib` hard-errors on a bin-only package — which it then
reads as "mutant caught".

**The gate asks whether to believe the sweep before it asks whether the sweep
is within budget**, and skips the budget comparison entirely when the answer is
no. This is the most important thing in this document. On 2026-06-14 the
recorded sweep reported `kmuxd` 712 caught / 0 missed, `kmux-gtk` 608/0 and
`kmux` 15/0 — a perfect score for 24k lines that had never been mutation-tested
at all, because `cargo test --package=kmuxd --lib` fails in a tenth of a second
with "no library targets found" and cargo-mutants read that failure, correctly
by its own lights, as a catch. 1,320 of 2,592 "caught" mutants were fabricated,
and the number was cited as evidence the daemon was well covered.

The tell is timing: a caught mutant runs the whole test binary, so it takes
roughly as long as the sweep's own baseline; a mutant "caught" by a target that
does not exist takes no time at all. The gate flags any package with a perfect
score whose slowest catch is under a fifth of the baseline (or, with no usable
baseline, under a second). The ratio is self-calibrating, so there is no
per-crate threshold to tune. `mise run mutants-gate --write` refuses to record
a sweep it does not believe — recording a fabricated 100% would enshrine it.

Budgets are absolute `missed` counts, not percentages, so adding well-tested
code to a crate cannot fail an unrelated PR by moving a ratio. Only crates a
sweep actually covered are judged, so a sharded or scoped run says nothing
about the rest instead of reporting them all as stale.

Per PR, CI mutates **the diff only** (`--in-diff`): it needs no baseline and
cannot go stale, because its scope *is* the change under review. It can be
skipped with a `skip-mutants` label, which is visible on the PR.

Scoping flags are forwarded to every crate-group pass and echoed on each `==>`
line, so a CI log says what was actually swept rather than implying it. A group
with nothing in scope passes; a group that exits non-zero having written no
outcomes fails, because it did not run and its crates are unmeasured.

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
| `mise run mutants-gate` | believability + the mutation ratchet |
| `mise run deps-audit` | licences, advisories, unused dependencies |
| `RUSTDOCFLAGS=-D warnings mise run doc-check` | documentation |

`lint-gate` subsumes `clippy`, so CI runs the gate and not both. The pre-push
hook still runs `clippy`, which is the faster check and the one that catches the
common case.
