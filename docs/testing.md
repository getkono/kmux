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
`session_manager/mod.rs` once did for the single-file `server_handler.rs`) has
outgrown its module.
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
*Enforced by:* `clippy::disallowed_methods` — `clippy.toml` lists
`std::env::set_var` and `std::env::remove_var`, so a new site fails
`mise run clippy` / `mise run lint-gate` — plus the audit snippets below.

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
  `advance_blink(phase_start, now)`, `TimeoutEnforcer::check(now)`. A trait
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

Measured 2026-08-22; the R3 row updated 2026-09-25 (issue #204):

| Rule | At branch start | Now | Target |
| --- | --- | --- | --- |
| R3 — no process-global env mutation | 91 sites / 13 files | **0 / 0** | 0 — reached |
| R3/R13 — no test-only lock | 98 sites / 10 files | **0 / 0** | 0 — reached |
| R4 — no function over 100 lines | 45 (largest 888 lines) | 45 (largest 394, a registered exception) | 0, minus the exceptions register |
| R5 — no double in a release build | 2 (`kmux-pty`'s `pub mod mock`) | **0** | 0 — reached |
| R12 — mutation score is the coverage bar | 3 crates fabricated, 5 never swept | scoring fixed; **no trustworthy sweep yet** | a recorded `[[mutants]]` budget per crate |

Every case reached zero the same way — take the thing the test needs to vary and
make it a parameter:

- `kmux-sys::dirs` (then `kmux-protocol::dirs`) — the `Dirs` value replaced twelve unsafe environment
  overwrites and the module's own lock; 8 serialised tests became 17
  parallel-safe ones.
- `kmux-app::config` — the eight resolvers now take `&KmuxConfig`, so a test
  constructs a config value instead of writing a file and pointing
  `XDG_CONFIG_HOME` at it; 28 tests became 32, all parallel-safe.
- The four `kmuxd` e2e suites — a `Sandbox` value hands the child daemon its
  `XDG_*` through `Command::env` and resolves the test's own paths through
  `Dirs::rooted` at the same root, so the four `ENV_LOCK`s went with it. That
  lock never bought isolation anyway: it serialised tests within one binary
  while two `cargo test` processes still shared the real `$XDG_RUNTIME_DIR`.
- The last five sites (issue #204) — `kmuxd::auth::persist_token_in` takes a
  `&Dirs` (the test passes `Dirs::rooted`; `persist_token` passes
  `Dirs::from_env()` and is itself checked in a child process);
  `kmuxd::impair` parses through `ImpairConfig::from_lookup`, which takes the
  variable lookup as a parameter (`from_env` passes `std::env::var`, the tests
  a table, and `from_env` itself is checked in a child process handed the
  knobs through `Command::env`); and `kmux-sys::identity`'s test, which
  duplicated the `load_or_create_at(path)` tests beside it, was deleted. With
  the count at zero, R3 graduated to a hard gate: `clippy.toml`'s
  `disallowed-methods`.

R4's count did not fall, and that is the honest reading: the three god
dispatchers were split, but what each leaves behind is a flat, exhaustive
`match` that is still over 100 lines — 378 for `handle_server_message`, 260 for
`handle_message`, 180 for `dispatch_action`. Keeping them flat is deliberate:
exhaustiveness is what makes adding a message variant fail the build until
someone decides what to do with it, and delegating to per-domain sub-routers
would trade that for a shorter function. What moved is what the lint is
actually a proxy for — logic per function. `handle_server_message` went from
761 lines of reconciliation to a table over 52 named handlers, and the
mutants that stand for "this message does nothing" went from 2 to 104.

R12 is the one row that is not yet a number. The scoring bug is fixed and the
believability check is in place, but a full sweep takes hours and none has run
since, so `[[mutants]]` is empty and the per-PR CI job mutates only the diff —
which needs no baseline, because its scope *is* the change under review, and
which `mutants-gate --diff` holds to zero survivors whatever the table later
records. The
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
| `kmux-protocol` | 124 | — | codec byte fixtures, framing, version/capability negotiation, message categories, compat classification; the control socket's `ControlError` wire form (issue #207) | wire fixtures — the crate is pure data, so every test is tier *pure* | — |
| `kmux-sys` | 64 | — | XDG path resolution rules, Ed25519 identity round-trip, TOFU store, transport constants; a stalled TLS handshake does not block the next accept, and times out; a QUIC connection establishes off the accept with its control stream; the accept loop backs off after a failed accept (issue #206, loopback sockets, paused clock); handshake admission (issue #207): the per-source and listener-wide bounds refuse at once rather than queue, slots come back when a handshake ends, zero bounds are refused at construction, IPv6 sources group by /64 (pure `InFlight` tests plus a scripted `Listener` on the paused clock), and on loopback QUIC a dropped attempt is refused at once, a Retry brings the client back validated, and `serve` admits a retried client. The crate re-lists itself as a dev-dependency with its transport and `identity` features, so `cargo test -p kmux-sys` — and so cargo-mutants — builds the listeners these tests cover | `Dirs::rooted` | keyring |
| `kmux-app` | 300 | — | action dispatch, mode resolution, layout geometry, config resolution, command registry, driver tick | `AppCore::for_test`, `FrontendDriver::for_test` | `run_cli` process exit |
| `kmuxd` | 174 | 18 | message handlers, app state, relay, auth, wordlist, persistence; grid conformance (R10); 5 e2e suites; backpressure and lock discipline (issue #206): input queue full without blocking the `sessions` lock, bounded outbound queue with lag → `SyncReset` resync, write timeout, auth/pong deadlines (pure verdicts + paused-clock watchdog), per-hold relay byte cap, `term_state` poison recovery, task supervisor; deadlines (issue #207): the control socket's deadline and request cap with their typed error replies (paused clock and `UnixStream::pair`), a `restart` cut off before hand-over rolling back its busy mark (at the `BegunHandoff` guard), a non-UTF-8 request refused as malformed, a stalled QUIC pane stream reset alone with a `Lagged` resync while the connection stays up (loopback QUIC, real clock, a 200 ms stall timeout); a lagged broadcast forwarder sends a resync session list and keeps running (issue #208, paused clock) | `crate::fixtures` (`fixture_app`, `fixture_client_state`, `NoopAttacher`, …), `NullEventSink` (via `kmux-vt-core/test-util`); e2e: `harness::{Sandbox, Daemon, Federation}` | fork/exec, `SCM_RIGHTS`, daemonize, `startup::async_main` |
| `kmux-client` | 163 | 3 | server-message handling, grid apply, selection, input, liveness; grid-apply proptest (R10); a session list as the whole truth (issue #208): unlisted sessions close, the viewed tab follows its listed layout | channel injection | — |
| `kmux-connect` | 85 | — | bootstrap racing, daemon lifecycle, token handling, host parsing, attach-gate refusals; a control `ControlError` reply read as an `Err` naming the refusal (issue #207) | `Dirs::rooted` | real sshd handshake, QUIC/TLS on the wire |
| `kmux-vt-core` | 71 | — | diff engine, scrollback mirror, backend contract | `MockBackend`, `NullEventSink` (`test-util`) | real terminal emulation |
| `kmux-render` | 54 | — | geometry, packed format, atlas packing, colour, dirty-row parity | — | GPU adapter (skips cleanly, R11) |
| `kmux-pty` | 54 | — | timeout policy, registry, expect parser, size math; process hygiene (issue #205): close-on-exec masters, what a child inherits (fds, cwd, signal state), start failures, the reaper, process-group close (and none for an exited child), fd count across close cycles; a failed reaper start is retried by the next spawn and a success kept (issue #207, `get_or_start` with an injected start) | `fixtures::wait_until_dead` (`MockPty` deleted: 114 lines of `tokio::io::duplex` wrapper with no consumer) | termios |
| `kmux-ghostty` | 26 | — | safe façade, `Send`/`Sync` static assertions, event decode | `NullSink` | libghostty internals |
| `kmux-ffi` | 17 | — | a few leaf conversions | — | `extern "C"` dispatch, uniffi object lifetimes |
| `kmux-gtk` | 14 | — | keyval→protocol conversion, accel→action table | — | **all widget construction and the glib main loop** |
| `kmux-vt-worker` | 0 | 1 | subprocess smoke: PTY output becomes diffs, a heartbeat `Ping` is answered, and a `Hold` parks the PTY reader until `Release` (issue #207) | — | fd adoption over `SCM_RIGHTS` |
| `kmux-ghostty-sys` | 6 | — | ABI version constant | — | Zig internals, all raw bindings |
| `kmux-worker-protocol` | 6 | — | postcard roundtrip, version constant | — | — |
| `kmux` | 6 | 6 | CLI parse, completion, diagnostic, binary location | real-binary invocation | `exec` of the platform frontend |

Swift: `kmux-swift/Tests/KmuxAppTests/` — 11 `func test`, run by
`mise run swift-test` in the macOS CI job. It is the coverage for `kmux-ffi`'s
untestable half.

## Shared fixtures and helpers

Reach for these before writing a setup of your own; a second copy of one is the
duplication they exist to remove.

- **`kmuxd`'s `#[cfg(test)] mod fixtures`** (`crates/kmuxd/src/fixtures.rs`,
  R5): `fixture_app()` (an empty `ServerApp` accepting `FIXTURE_TOKEN`),
  `fixture_client_state(app, transport)` (a connection's `SharedClientState`,
  its outbound compressor and the receiving end of its outbound queue),
  `NoopAttacher`, `make_outbound()` (a default-sized outbound queue whose
  overflow closes nothing — the stand-in for a client's control channel
  wherever a test builds a `ClientSender` or viewer by hand),
  `fixture_term_state(rows, cols)`, `sample_grid()` and
  `sample_persisted_session(word, name, last_active_ms)`. The dispatch tests'
  `testing` module re-exports `FIXTURE_TOKEN`, `fixture_app`,
  `fixture_client_state` and `NoopAttacher`.
- **`kmux-pty`'s `fixtures::wait_until_dead(pid, deadline)`** polls
  `kill(pid, 0)` and returns whether the process is gone from the process
  table. It replaces a fixed sleep before a liveness assertion: a dying process
  is seen as soon as it is gone, and a deadline already passed is a single
  probe. It never reaps, so a zombie counts as alive: waiting on it asserts
  the child was both killed and reaped (by whichever task owns the `waitpid`).
- **The `kmuxd/tests/harness`**: `Sandbox` (a private XDG root, R3),
  `Daemon` (spawn one into a sandbox), `Federation` (`spawn_pair()` starts a
  remote and a local hub; `open_peer()` federates them through a connected GUI
  and returns it with the peer id), and `E2E_TIMEOUT`, the one bound on every
  e2e wait. A shorter bound needs a comment saying why.
- **`kmux-vt-worker` is a build prerequisite, not a test step.** `cargo test`
  builds only a package's own binaries, so `mise run test` depends on the
  `build-vt-worker` task and the harness only locates the binary, failing with
  the command to run when it is missing. Running `cargo test -p kmuxd` by hand
  needs `cargo build -p kmux-vt-worker` first.

## Running

```sh
mise run test                          # the whole workspace; matches CI (builds kmux-vt-worker first)
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
cargo-mutants misreads the error as a caught mutant. An unscoped run makes one
pass per group into `mutants.out/{lib,bin,bin-fast}/mutants.out/` — one level
deeper than the group name, because cargo-mutants' `-o` names the *parent* and
creates `mutants.out/` inside it. All gitignored.

Every flag reaches all three passes, and each `==>` line echoes the flags it was
given. That is there because they once did not: the wrapper forwarded only the
crate list, so `--in-diff` and `--shard` were silently dropped — the per-PR job
swept the whole workspace instead of the diff, and the weekly sweep's eight
shards each ran the same full sweep. Both produced valid-looking results, which
is the failure mode this document exists to distrust.

A mutant that hangs a test is recorded as a timeout, which counts as caught,
but it costs the whole per-mutant timeout, and the per-PR job has an hour. Two
rules keep hung mutants cheap:

- **A test that waits, waits bounded.** Every wait on a channel, a socket or a
  task sits under `tokio::time::timeout`, and a polling loop sleeps rather than
  `yield_now`s — on the paused clock a yield loop never lets time advance, so
  the bound never passes. A half-closed `tokio::io::duplex` (`shutdown()` the
  write half) gives a reader EOF where a merely dropped `WriteHalf` does not.
- **No unit test installs a process-wide signal handler.** cargo-mutants stops
  a timed-out test binary with `SIGTERM`. Once any test in the binary has
  called `tokio::signal::unix::signal(SignalKind::terminate())` or
  `tokio::signal::ctrl_c()`, that signal no longer kills the process: the hung
  binary runs on as an orphan, and one spinning in its mutant starves every
  later build on the runner. That is how #206's mutation job overran its hour —
  a control-socket test installed the daemon's handlers, and build times grew
  sixfold as orphans piled up. The handlers are installed only by
  `daemon::termination_signal`, which the daemon passes in and tests replace.
  A handoff is stopped for a shutdown by a `handoff::Cancel` signal the daemon
  fires on SIGTERM, so its tests fire one by hand instead of sending a signal.

A group with nothing to test under `--in-diff` or `--shard` says so and passes;
a group that exits non-zero having written no outcomes did not run, and fails
the sweep. Those two look identical from the filesystem alone.

**Always read a sweep through `mise run mutants-gate`, never straight off the
summary line.** The gate's first job is deciding whether the sweep can be true
at all: it flags any package with a perfect score where no caught mutant's log
shows the test harness starting (`running N tests`), which is the signature of a
test command that failed before it ran anything. That is not a hypothetical — it is
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
| `kmuxd::startup::async_main` (383 lines) | A linear boot script — bind, TLS, handoff, listeners, signals. Every split yields a function nothing can assert on without a live daemon. Exempt from R4, and its body-replacement mutant is excluded by `exclude_re` in `.cargo/mutants-bin.toml` like `fn main`'s (issue #206) | the five `kmuxd/tests/*_e2e.rs` suites |
| `kmuxd` fork/exec, `SCM_RIGHTS`, daemonize | Cannot run in-process | `handoff_e2e.rs`, `process_isolation_e2e.rs` |
| `kmuxd`'s `fn main` | A process entrypoint (CLI parse, daemonize, runtime build and teardown) no unit test can call, so its body-replacement mutant is always missed under `--bins`; excluded by `exclude_re` in `.cargo/mutants-bin.toml` so a comment edit in it does not fail the zero-survivor diff job (issue #205) | the five `kmuxd/tests/*_e2e.rs` suites, which spawn the binary |
| `kmux-app`'s `fetch_remote_logs` (`kmux daemon logs --server`) | Resolves, connects to and authenticates with a remote daemon before streaming; with no daemon to reach, its body-replacement mutant is always missed, so `exclude_re` in `.cargo/mutants.toml` excludes it (issue #206) | `stream_logs`, the stream loop it hands off to, is unit-tested over `tokio::io::duplex` |
| `kmux-app`'s `tail_local_log` (`kmux daemon logs` / `kmux client logs` on this machine) | Hands stdout to `tail_local_log_to`; its `Ok(())` mutant is observable only on the process's stdout, so `exclude_re` in `.cargo/mutants.toml` excludes it (issue #207) | `tail_local_log_to`, driven with a buffer; the rotation-following read (`kmux_sys::log_tail::read_appended`) is unit-tested in `kmux-sys` |
| `kmux-app`'s `run_daemon_command` and `probe_takeover` (`kmux daemon …`) | The dispatcher's arms talk to this profile's real daemon and print to stdout, and `probe_takeover` is the one look at the real daemon a restart polls; their mutants are excluded by `exclude_re` in `.cargo/mutants.toml` (issue #207) | the restart's logic — `wait_for_takeover` (paused clock, scripted probes), `report_takeover`, `stood_down` — is unit-tested; `handoff_e2e.rs` runs a real restart |
| `kmux-connect`'s `query_handoff` | Resolves this profile's control socket, then calls `query_handoff_at`; its `Ok(Default::default())` mutant is excluded by `exclude_re` in `.cargo/mutants.toml` (issue #207) | `query_handoff_at`, against a stand-in control socket |
| `kmuxd`'s `handoff::sender::run` and `hand_off` | Bind the real handoff socket and spawn a successor daemon, so no unit test can call them; their body-replacement mutants are excluded by `exclude_re` in `.cargo/mutants-bin.toml` (issue #207) | `handoff_e2e.rs`, which runs a real handoff end to end and a successor that stands down; the pieces they call are unit-tested where they live: the protocol drive (`drive`, over a socket pair against a scripted successor, including a shutdown signal before the `Ack` is read), `await_successor` and `Successor::stop` (against real `sh`/`sleep` children, never the test process itself), `resolve_successor_exe` in `handoff/sender.rs`, the frame codec, `Cancel` and `peer_pid` in `handoff/mod.rs`, the pane-creation gate in `app/migrate.rs`, and the final checkpoint write (`Checkpointer::write_final_from`) in `persist/checkpoint.rs` |
| `kmuxd`'s `handoff::receiver::run` | Resolves the real handoff and control socket paths; with no predecessor to reach it waits out `PREDECESSOR_CONNECT` | `handoff_e2e.rs` (a successor with no handoff socket stands down, exit code 75, while a daemon serves); `pull`, `unreachable_predecessor` and `connect_with_retry` are unit-tested on the paused clock, with every `Abort` branch driven against a predecessor already gone |
| `kmuxd`'s `RotatingFile::flush` | An equivalent mutant: it forwards to `File::flush`, which does nothing for an unbuffered `File`, so `Ok(())` is the same function; excluded by `exclude_re` in `.cargo/mutants-bin.toml` (issue #207) | — |
| `kmux-connect` real sshd handshake | Needs a live sshd in CI | `PeerTarget::Direct`, added precisely so federation is e2e-testable without sshd — see [architecture-federation.md](architecture-federation.md) |
| `kmux-pty` `forkpty` and real child spawn | Process and tty syscalls. The pre-`execve` child code (`child.rs`) runs in a forked process, so it is observed only through what the program then sees | `kmux-pty`'s own tests spawn real children, observe them from inside (`ls /dev/fd`, `pwd`, `yes \| head`) and wait on them with `wait_until_dead`; the `kmuxd` e2e suites spawn real shells |
| `kmux-render` GPU adapter | No adapter on a headless runner | the pure tier always runs; GPU smoke skips cleanly (R11) |
| `kmux-ghostty-sys` Zig internals and raw bindings | Not Rust; excluded from mutation by `exclude_globs` | `EXPECTED_ABI_VERSION` (R8); `kmux-vt-core`'s diff tests |
| `KMUX_FFI_ABI_VERSION` bump on a surface change | Not machine-detectable | human review; the generated-bindings diff |
