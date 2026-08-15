# Testing

This document is **normative**. It is the single place that records how kmux is
tested: where a test lives, what it must assert, how behaviour is injected
instead of mutated into the process, where test doubles belong, what the bar is
per crate, and which areas are deliberately not tested.

Read it before you add a test, a test double, or a `#[cfg(test)]` seam. If the
tree needs to stop obeying a rule below, change the rule here in the same commit
that breaks it — a rule nobody updated is worse than no rule.

Two companions: [crate-usage.md](crate-usage.md) governs dependencies (including
where `tempfile` and `proptest` may be declared), and
[architecture-verification.md](architecture-verification.md) describes the
grid-digest oracle in depth — this document points at it rather than repeating
it.

## Why mutation score, not line coverage

A line-coverage number says a line *ran*. It cannot say an assertion would have
noticed if that line were wrong. kmux measures coverage by **mutation score**:
`cargo mutants` rewrites the code — flipping a comparison, replacing a function
body with a constant — and a mutant that survives the test suite is a line the
suite executes but does not check.

The 2026-06-14 sweep found **1,177 of 1,434 surviving mutants were "replace
function body with a constant"** — functions whose return value nothing asserts
on. That single number is why R2 and R4 exist, and it is invisible
to line coverage, which those functions score 100% on.

## Rules

Each rule carries what enforces it. A rule with no enforcement is an aspiration,
and this project has already learned what those are worth: `crate-usage.md` R5
declared the crate layering normative for months while nothing checked it.

**R1 — tests live beside the code they test.** A `#[cfg(test)] mod tests` in the
same file as its subject. `tests/` is only for a suite that must cross the
crate's public API or a process boundary. A unit test in `tests/` is a misfiled
unit test, and a subject whose tests live in a *different* file (as
`session_manager/mod.rs` does for `server_handler.rs`) has outgrown its module.
*Enforced by:* review.

**R2 — every test asserts on a value.** A test whose only claim is that a call
returned is a compile check, not a test. Assert on the return value, and for a
`&mut self` subject also on at least one accessor. This is the direct counter to
the dominant survivor class. *Enforced by:* the mutation ratchet (R12) —
a surviving body-replacement mutant is exactly the report that a return value
went unasserted.

**R3 — behaviour enters through parameters, not through the process.** No
`std::env::set_var` under `crates/`. Paths come from a `Dirs` value
(`Dirs::rooted(tmp)` in tests); time comes from a `now: Instant` parameter; a
child process is configured with `Command::env`, never by mutating the parent's
environment. Process-global mutation forces tests to serialise on a lock, and a
per-file lock does not serialise against another file's.
*Enforced by:* `clippy::disallowed_methods`, plus the audit snippets below.

**R4 — one match, many handlers.** A dispatcher over a message or action enum
keeps a router whose arms only destructure and call. Each arm's logic is a named
`on_*` function taking the *payload* and returning the router's effect type —
never `()`. Handlers are grouped one module per message family, not one file per
arm. Keep the `match` (the compiler's exhaustiveness check is load-bearing); do
not replace it with a runtime registry. *Enforced by:* `clippy::too_many_lines`.

> A body-replacement mutant is generated **per function**. An 888-line
> `handle_message` therefore yields *one* mutant covering 50 message types — any
> single test kills it and the other 49 arms are invisible to the tool. Fifty
> handlers yield fifty mutants, each killable only by an assertion specific to
> that message. Splitting a dispatcher is not primarily a readability change; it
> is what gives the metric enough resolution to see the code at all.

**R5 — test doubles are feature-gated and never reachable from a release
build.** A double used by another crate lives behind that crate's `test-util`
feature and is consumed through `[dev-dependencies]`; `kmux-vt-core`'s
`NullEventSink` is the reference implementation. A double used only in its own
crate lives in a `#[cfg(test)] mod fixtures`. A double with **no** consumer is
deleted, not gated: `kmux-pty`'s `MockPty` was 114 lines wrapping
`tokio::io::duplex`, shipped in every release build, and had never been used by
anything — feature-gating it would only have hidden that. *Enforced by:* the
audit snippet below.

**R6 — naming.** `fixture_*()` builds the state under test; `make_*()` /
`sample_*()` build a value. Test functions are
`<subject>_<condition>_<expectation>`, snake_case, no `test_` prefix. Doubles are
`Null*` (does nothing), `Recording*` (captures calls), `Scripted*` (replays a
canned sequence), and `Mock*` only where a real implementation is emulated
(`MockBackend`). *Enforced by:* review.

**R7 — climb the tiers only when forced.** Prefer a pure function to
`tokio::io::duplex`; prefer `duplex` to a real socket; prefer a fake `Listener`
to spawning `kmuxd`. A test that spawns a process states in a comment which
lower tier cannot cover it. *Enforced by:* review.

**R8 — anything crossing a version or process boundary carries a fixture
test.** The data-plane encoding (byte fixtures in `kmux-protocol`'s codec module
— `rmp-serde` is exact-pinned because the encoding *is* the protocol), the
daemon↔worker contract, `KMUX_FFI_ABI_VERSION`, `EXPECTED_ABI_VERSION`. A codec
change that breaks no fixture test is a change nobody tested. Mirrors R6 of
[crate-usage.md](crate-usage.md). *Enforced by:* the fixture tests themselves.

**R9 — invariants over examples where the space is enumerable.** Registries,
keymaps and enum mappings get one exhaustive or invariant test, not three spot
checks. `kmux-app`'s command registry (`no_duplicate_canonical_names`,
`no_duplicate_aliases`, `usage_strings_well_formed`) is the reference.
*Enforced by:* review.

**R10 — two implementations ship with the differential test that pins them
together.** `kmuxd/tests/grid_conformance.rs` (server VT vs. client `CellGrid`,
compared by digest) and `kmux-client/tests/grid_apply_worker.rs` (worker-backed
vs. synchronous grid, proptest) are the references. A new second implementation
of existing behaviour lands with its differential test in the same commit.
*Enforced by:* review; see
[architecture-verification.md](architecture-verification.md).

**R11 — hardware and toolkit tiers skip cleanly, never fail.** A test needing a
GPU adapter, a real PTY or a display detects absence and skips —
`kmux-render`'s `try_renderer` returning `None` on `RenderError::NoAdapter` is
the reference. The tier below it runs unconditionally in CI.
*Enforced by:* CI (a headless runner exercises the skip path every build).

**R12 — mutation score is the coverage bar.** Run
`mise run mutants -- -p <crate>` before merging a change to that crate. A
surviving mutant is either killed by a new assertion or recorded in
[Known exceptions](#known-exceptions) with a reason. Scores may only improve:
the per-crate budget lives in `quality-baseline.toml`.
*Enforced by:* the mutation ratchet.

**R13 — tests run in parallel, in any order, in one binary.** No process-global
mutation, no shared fixture file, no test-only mutex. A test that needs a mutex
is a design defect and the mutex is its bug report. *Enforced by:* R3's
lint, and by the default `cargo test` thread pool.

**R14 — a bug fixed is a `fix(scope):` commit of its own**, carrying the test
that fails without it. Never fold a bugfix into a `refactor:` commit: git-cliff
routes `fix` to *Bug Fixes* and `refactor` to *Refactor*, so a hidden bugfix is
invisible in both the changelog and the review. *Enforced by:* review; see
[releasing.md](releasing.md).

### Deliberately rejected

Recorded so they are not re-proposed:

- **A `Clock` trait.** The repo already injects time the idiomatic way — at the
  pure boundary, as a parameter: `Liveness::{observe_inbound, is_timed_out}(now)`,
  `advance_blink(phase_start, now)`, `TimeoutPolicy::check(started_at)`. A trait
  would add a generic or a `dyn` field to four large structs and buy nothing
  `Instant` arithmetic does not already give a test. R3 is the rule; a
  trait is not.
- **Dependency-injection traits in `kmux-app` / `kmux-client`.**
  `AppCore::for_test` and `FrontendDriver::for_test` (which hands back the
  server-message and bootstrap channels) already give construction plus
  injection, and `SessionManager`'s outbound path is already an `mpsc` sender —
  a channel is a better fake than a trait. The missed mutants in these crates
  come from oversized functions whose arms return an uninformative constant,
  which no trait can fix. Pure-function extraction and R4 are the answer.
- **A filesystem/VFS trait.** `tempfile` plus `Dirs::rooted` covers every case
  without infecting every I/O call site.
- **A widget abstraction over GTK4.** It would be a second, untested UI
  framework. See [Known exceptions](#known-exceptions).

## Adoption status

The tree does not satisfy every rule above yet. That is stated here rather than
left implicit, because a normative document whose rules are quietly violated is
the problem this one exists to fix.

Measured 2026-08-15:

| Rule | At branch start | Now | Target |
| --- | --- | --- | --- |
| R3 — no process-global env mutation | 91 sites / 13 files | **17 / 9** | 0 |
| R3/R13 — no test-only lock | 98 sites / 10 files | **36 / 6** | 0 |
| R4 — no function over 100 lines | 45 (largest 888 lines) | 45 | 0, minus the exceptions register |
| R5 — no double in a release build | 2 (`kmux-pty`'s `pub mod mock`) | **0** | 0 — reached |
| R12 — mutation score is the coverage bar | 3 crates fabricated, 5 never swept | scoring fixed; **no trustworthy sweep yet** | a recorded `[[mutants]]` budget per crate |

Two modules have reached zero on R3 and R13, both by the same move — take the
thing the test needs to vary as a parameter:

- `kmux-protocol::dirs` — the `Dirs` value replaced twelve unsafe environment
  overwrites and the module's own lock; 8 serialised tests became 17
  parallel-safe ones.
- `kmux-app::config` — the eight resolvers now take `&KmuxConfig`, so a test
  constructs a config value instead of writing a file and pointing
  `XDG_CONFIG_HOME` at it; 28 tests became 32, all parallel-safe.

R12 is the one row that is not yet a number. The scoring bug is fixed and the
believability check is in place, but a full sweep takes hours and none has run
since, so `[[mutants]]` is empty and the per-PR CI job mutates only the diff —
which needs no baseline, because its scope *is* the change under review. The
weekly sweep is what fills the table in. Recording the June numbers instead
would have been worse than recording nothing.

These are budgets, not aspirations: each one is recorded in
`quality-baseline.toml` and may only shrink. CI fails both when a count rises
*and* when a count falls without the budget being tightened, so the gap closes
monotonically and cannot silently reopen. A rule reaches zero, its budget row is
deleted, and its check graduates from a ratchet to a hard gate.

## The tiers

| Tier | May use | Example |
| --- | --- | --- |
| **pure** | values only; no I/O, no clock, no spawn | `kmux-app`'s layout geometry, `kmux-render`'s scene building |
| **in-memory** | `tokio::io::duplex`, channels, fakes, `tempfile` + `Dirs::rooted` | `kmux-protocol`'s codec roundtrip, `kmuxd`'s client-session loop |
| **in-process** | a real runtime, real fds, a real VT — but no second process | `kmuxd/tests/grid_conformance.rs` |
| **out-of-process** | spawns a real binary | `kmuxd/tests/handoff_e2e.rs`, `kmux-vt-worker/tests/worker_smoke.rs` |

Every tier above *pure* states in a comment why the tier below cannot cover it
(R7).

## Per crate

Counts are `#[test]` + `#[tokio::test]` functions, measured 2026-08-16.

| Crate | Unit | Integ | What is tested | Doubles & seams | Not tested (→ exceptions) |
| --- | --- | --- | --- | --- | --- |
| `kmux-protocol` | 124 | — | codec byte fixtures, framing, version/capability negotiation, message categories, compat classification | wire fixtures — the crate is pure data, so every test is tier *pure* | — |
| `kmux-sys` | 51 | — | XDG path resolution rules, Ed25519 identity round-trip, TOFU store, transport constants | `Dirs::rooted` | real sockets, real TLS handshakes, keyring |
| `kmux-app` | 300 | — | action dispatch, mode resolution, layout geometry, config resolution, command registry, driver tick | `AppCore::for_test`, `FrontendDriver::for_test` | `run_cli` process exit |
| `kmuxd` | 174 | 18 | message handlers, app state, relay, auth, wordlist, persistence; grid conformance (R10); 5 e2e suites | `NoopAttacher`, `ServerApp::new`, `NullEventSink` (via `kmux-vt-core/test-util`) | fork/exec, `SCM_RIGHTS`, daemonize, `startup::async_main` |
| `kmux-client` | 163 | 3 | server-message handling, grid apply, selection, input, liveness; grid-apply proptest (R10) | channel injection | — |
| `kmux-connect` | 85 | — | bootstrap racing, daemon lifecycle, token handling, host parsing, attach-gate refusals | `Dirs::rooted` | real sshd handshake, QUIC/TLS on the wire |
| `kmux-vt-core` | 71 | — | diff engine, scrollback mirror, backend contract | `MockBackend`, `NullEventSink` (`test-util`) | real terminal emulation |
| `kmux-render` | 54 | — | geometry, packed format, atlas packing, colour, dirty-row parity | — | GPU adapter (skips cleanly, R11) |
| `kmux-pty` | 34 | — | timeout policy, registry, expect parser, size math | — (`MockPty` deleted: 114 lines of `tokio::io::duplex` wrapper with no consumer) | `forkpty`, real child spawn, termios |
| `kmux-ghostty` | 26 | — | safe façade, `Send`/`Sync` static assertions, event decode | `NullSink` | libghostty internals |
| `kmux-ffi` | 17 | — | a few leaf conversions | — | `extern "C"` dispatch, uniffi object lifetimes |
| `kmux-gtk` | 14 | — | keyval→protocol conversion, accel→action table | — | **all widget construction and the glib main loop** |
| `kmux-vt-worker` | 0 | 1 | subprocess smoke | — | fd adoption over `SCM_RIGHTS` |
| `kmux-ghostty-sys` | 6 | — | ABI version constant | — | Zig internals, all raw bindings |
| `kmux-worker-protocol` | 6 | — | postcard roundtrip, version constant | — | — |
| `kmux` | 6 | 6 | CLI parse, completion, diagnostic, binary location | real-binary invocation | `exec` of the platform frontend |

Swift: `kmux-swift/Tests/KmuxAppTests/` — 11 `func test`, run by
`mise run swift-test` in the macOS CI job. It is the coverage for `kmux-ffi`'s
untestable half.

## Running

```sh
mise run test                          # the whole workspace; matches CI
cargo test -p kmux-app                 # one crate
mise run swift-test                    # the native macOS app
cargo test -p kmux-render --features gpu   # the GPU tier (skips with no adapter)

mise run mutants                       # full sweep, one pass per crate group
mise run mutants -- -p kmux-protocol   # one crate (config auto-selected)
mise run mutants -- --in-diff pr.diff  # only mutants on changed lines
mise run mutants-gate                  # judge the sweep, then check the budget
```

Mutation configuration lives in `.cargo/mutants*.toml` — three files, one per
crate target shape, because `--lib` hard-errors on a bin-only package and
cargo-mutants misreads the error as a caught mutant. Results land in
`mutants.out/` (gitignored).

**Always read a sweep through `mise run mutants-gate`, never straight off the
summary line.** The gate's first job is deciding whether the sweep can be true
at all: it flags any package with a perfect score whose slowest catch finished
in a fraction of the sweep's own baseline, which is the signature of a test
command that failed before it ran anything. That is not a hypothetical — it is
how 1,320 of the June sweep's 2,592 "caught" mutants came to be fabricated. When
it fires, the budget comparison is skipped entirely, and `--write` refuses to
record the sweep. See [docs/quality-gates.md](quality-gates.md).

## Auditing

Run from the repository root. Each snippet's target output is empty; until it is,
the current count is a budget in `quality-baseline.toml` that may only shrink
(see [Adoption status](#adoption-status)).

```sh
# R3 — process-global environment mutation. Target: no output.
git grep -n 'env::set_var\|env::remove_var' -- 'crates/**/*.rs'

# R3/R13 — the lock that only exists to serialise env mutation. Target: no output.
git grep -n 'await_holding_lock\|ENV_LOCK' -- 'crates/**/*.rs'

# R5 — a test double reachable from a release build. Target: no output.
git grep -n '^pub mod mock\|^pub mod fixtures\|^pub use mock' -- 'crates/*/src/lib.rs'

# R4 — function bodies over 100 lines, by brace depth. Every hit must be either
# split or listed in Known exceptions. Braces inside strings and comments are
# miscounted, so this is a review aid; clippy::too_many_lines is the gate.
awk '/^ *(pub )?(pub\(crate\) )?(async )?(unsafe )?fn / && !infn {
         infn=1; start=FNR; depth=0; opened=0 }
     infn { o=gsub(/\{/,"&"); c=gsub(/\}/,"&"); depth += o - c
            if (o > 0) opened=1
            if (opened && depth <= 0) {
                if (FNR-start > 100) printf "%s:%d: %d lines\n", FILENAME, start, FNR-start
                infn=0 } }' \
  $(git ls-files 'crates/*/src/*.rs' 'crates/*/src/**/*.rs') | sort -t: -k3 -rn
```

## Known exceptions

Each row is an area deliberately left untested, with what covers it instead.
Adding a row is a normative change: justify it in the commit that adds it.

| Area | Why | Covered instead by |
| --- | --- | --- |
| `kmux-gtk` widget construction and the glib main loop | Needs a display server and GTK's callback graph; a widget abstraction would be a second untested UI framework | pure conversions in `imp/convert.rs` and `imp/actions.rs`; manual QA; `./kmux` |
| `kmux-ffi` `extern "C"` dispatch and uniffi object lifetimes | The boundary is generated; asserting on it tests uniffi, not kmux | `mise run swift-test` on macOS CI; `KMUX_FFI_ABI_VERSION` under R8 |
| `kmuxd::startup::async_main` (396 lines) | A linear boot script — bind, TLS, handoff, listeners, signals. Every split yields a function nothing can assert on without a live daemon. Exempt from R4 | the five `kmuxd/tests/*_e2e.rs` suites |
| `kmuxd` fork/exec, `SCM_RIGHTS`, daemonize | Cannot run in-process | `handoff_e2e.rs`, `process_isolation_e2e.rs` |
| `kmux-connect` real sshd handshake | Needs a live sshd in CI | `PeerTarget::Direct`, added precisely so federation is e2e-testable without sshd — see [architecture-federation.md](architecture-federation.md) |
| `kmux-pty` `forkpty` and real child spawn | Process and tty syscalls | `MockPty`; the `kmuxd` e2e suites spawn real shells |
| `kmux-render` GPU adapter | No adapter on a headless runner | the pure tier always runs; GPU smoke skips cleanly (R11) |
| `kmux-ghostty-sys` Zig internals and raw bindings | Not Rust; excluded from mutation by `exclude_globs` | `EXPECTED_ABI_VERSION` (R8); `kmux-vt-core`'s diff tests |
| `KMUX_FFI_ABI_VERSION` bump on a surface change | Not machine-detectable | human review; the generated-bindings diff |
