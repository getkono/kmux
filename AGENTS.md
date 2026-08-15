# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

Tasks run via `mise run <task>` (`mise tasks` lists them). Git hooks are managed
by `hk`; `mise install` (or `mise run setup`) installs them and initializes the
`vendor/ghostty` submodule.

- `mise run test` — workspace test suite (matches CI)
- `mise run clippy` / `clippy-fix` — lint / autofix
- `mise run fmt` / `fmt-check` — format / check
- `mise run build` — build with default features (incl. the GPU renderer);
  `build-no-gpu` builds the lean, wgpu-free path CI also checks
- `./kmux` — build + run the debug `kmux`, mirroring the installed binary:
  `./kmux` launches the GUI, `./kmux daemon status` runs a CLI subcommand. It
  forwards to `mise run dev`, which pins the debug daemon and routes per platform
  (native Swift app on macOS, `kmux-gtk` on Linux). One frontend is built — the
  renderer is a runtime config choice, not a separate build.

### Binaries

- `kmux` — toolkit-free entrypoint (CLI + launcher): `cargo run -p kmux`.
  `kmux open [user@host[:/path]]` is the explicit connect verb; the bare
  positional `kmux <host/path>` stays as a shorthand fallback.
- `kmuxd` — daemon: `cargo run -p kmuxd` (self-signed cert by default)
- `kmux-gtk` (GTK4, Linux + macOS) and `kmux-swift` (native macOS) are the
  clients. See [docs/building-macos.md](docs/building-macos.md) and
  [docs/architecture-frontend.md](docs/architecture-frontend.md) for setup.

### Runtime switches (default off, opt-in)

These are strictly-typed config/CLI, not environment variables.

- `renderer = "gpu"` in `~/.config/kmux/config.toml` — GPU terminal renderer
  (default `"cairo"`, i.e. Cairo/CoreText). Config-only on purpose: a kmux GUI
  client is a singleton process, so a per-launch flag could not retarget the
  already-running renderer. The render-debug overlay reports the *effective*
  renderer (a GPU-init failure falls back to CPU).
  See [docs/architecture-render.md](docs/architecture-render.md).
- `kmuxd --session-isolation process` (or `[daemon] session_isolation = "process"`
  in `kmuxd.toml`) — run each pane's VT pipeline in an isolated `kmux-vt-worker`
  subprocess. End users set the config key, since the daemon is auto-spawned.
  See [docs/architecture-process-isolation.md](docs/architecture-process-isolation.md).

### Diagnostics

- `kmux diagnostic [<test>]` — paint a render-verification pattern; `--emit`
  writes it to the host terminal. See [docs/architecture-render.md](docs/architecture-render.md).
- `kmux ls` / `kmux ps` (alias `top`) — list sessions / process overview.
  See [docs/architecture-process-overview.md](docs/architecture-process-overview.md).
- `kmux clients [<session>]` / `kmux kick <session> <client>` — list the client
  connections attached to sessions (with FRONTEND/BUILD columns) and detach one
  (issue #146). See [docs/architecture-identity.md](docs/architecture-identity.md).
- `kmux client status|logs|stop|restart` — manage the local singleton GUI client
  (mirror of `kmux daemon`); `status` warns on client↔daemon build/protocol skew.
  See [docs/architecture-identity.md](docs/architecture-identity.md).
- `kmux status [--format json]` — one health view across every kmux process:
  daemon, GUI client, this CLI, and isolated per-pane VT workers, with skew
  flagged via the shared `kmux-protocol::compat` SSoT. The scoped `daemon
  status` / `client status` stay the detailed views. Exits non-zero when the
  daemon is down or a blocking (protocol/profile) skew is present.
  See [docs/architecture-identity.md](docs/architecture-identity.md).
- `kmux notify` — from inside a pane, raise a native desktop notification on the
  GUI showing that session; clicking it refocuses the window + pane (issue #169).
  Reads `KMUX_PANE`/`KMUX_SESSION`; wired to Claude Code's `Stop`/`Notification`
  hooks. See [docs/architecture-claude-integration.md](docs/architecture-claude-integration.md).
- `kmux daemon logs` / `kmux client logs` — print a process log; `-n N` shows the
  last N lines, `-f/--follow` tails. `kmux daemon logs --server <host>` fetches a
  *remote* daemon's log over the data plane (issue #187); the local form and
  `client logs` read the file off disk. Unknown VT control sequences are logged
  here under the `kmux::vt` target.
  See [docs/architecture-vt-sequences.md](docs/architecture-vt-sequences.md).
- `kmux debug paths` — print the active profile's log/state/runtime paths.
  Debug builds isolate state under `kmux-debug/`.
  See [docs/profile-isolation.md](docs/profile-isolation.md).

## Conventions

- The client is layered so no UI toolkit is depended on at or below `kmux-app`:
  `kmux-protocol` → `kmux-client` → `kmux-app` (policy + `FrontendDriver` +
  `run_cli`) → frontends (`kmux-gtk`, `kmux-swift` via `kmux-ffi`); `kmux` sits
  on top. See [docs/architecture-frontend.md](docs/architecture-frontend.md).
- Document architectural changes in `docs/`.
- Strict Rust — no `#[allow(unused)]` without justification. A new suppression
  uses `#[expect(..., reason = "...")]`, which fails the build once it stops
  applying, rather than `#[allow]`, which never expires.
- Tests assert on values, take paths and time as parameters instead of mutating
  the process, and dispatchers split into named per-message handlers rather than
  one large `match`. The tiers, the per-crate methodology, test doubles, naming,
  and the register of intentionally-untested areas are normative in
  [docs/testing.md](docs/testing.md) — read it before adding a test, a test
  double, or a `#[cfg(test)]` seam.
- Conventional commits (`type: description`), enforced by the `commit-msg` +
  `pre-push` hk hooks and CI via `convco` (escape hatch: `git commit --no-verify`;
  check a range manually with `mise run commit-check <base>..HEAD`).
- `thiserror` for error types, `anyhow` for application-level errors. Which
  crate to use for every other job, where a version may be declared, and how
  optional deps are feature-gated are normative in
  [docs/crate-usage.md](docs/crate-usage.md) — read it before adding, removing,
  or upgrading a dependency.

## Correctness (IMPORTANT!)

- Every component that talks to an external dependency is versioned and rejects
  incompatible peers: the data protocol (`PROTOCOL_RANGE` plus named
  capabilities), the `kmux-ffi` C ABI
  (`KMUX_FFI_ABI_VERSION`), the daemon↔worker contract (`kmux-worker-protocol`),
  and `kmux-ghostty-sys` (`EXPECTED_ABI_VERSION`).
- Every connecting party proves a cryptographic identity (issue #146): the daemon
  challenges each `Auth` with a random nonce and verifies the Ed25519 signature
  against the presented public key (its SHA-256 fingerprint is the `machine_id`)
  before trusting it, so no client can impersonate another. The shared token
  still gates access; identity is layered on top for attribution + management.
  See [docs/architecture-identity.md](docs/architecture-identity.md).
- Coverage is measured by mutation score, not line count: `mise run mutants --
  -p <crate>`. A mutant surviving in code you changed is either killed by a new
  assertion or recorded, with a reason, in the Known-exceptions register in
  [docs/testing.md](docs/testing.md). Both lib and bin crates are scored — the
  flags must match each crate's target shape, or `--lib` hard-errors on a
  bin-only package and cargo-mutants misreads the error as a caught mutant. See
  `.cargo/mutants*.toml`.
