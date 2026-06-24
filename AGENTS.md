# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

Tasks run via `mise run <task>` (`mise tasks` lists them). Git hooks are managed
by `hk` and installed by `mise install` (or `mise run setup`).

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

- `kmux` — toolkit-free entrypoint (CLI + launcher): `cargo run -p kmux`
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
  connections attached to sessions and detach one (issue #146).
  See [docs/architecture-identity.md](docs/architecture-identity.md).
- `kmux notify` — from inside a pane, raise a native desktop notification on the
  GUI showing that session; clicking it refocuses the window + pane (issue #169).
  Reads `KMUX_PANE`/`KMUX_SESSION`; wired to Claude Code's `Stop`/`Notification`
  hooks. See [docs/architecture-claude-integration.md](docs/architecture-claude-integration.md).
- `kmux debug paths` — print the active profile's log/state/runtime paths.
  Debug builds isolate state under `kmux-debug/`.
  See [docs/profile-isolation.md](docs/profile-isolation.md).

## Conventions

- The client is layered so no UI toolkit is depended on at or below `kmux-app`:
  `kmux-protocol` → `kmux-client` → `kmux-app` (policy + `FrontendDriver` +
  `run_cli`) → frontends (`kmux-gtk`, `kmux-swift` via `kmux-ffi`); `kmux` sits
  on top. See [docs/architecture-frontend.md](docs/architecture-frontend.md).
- Document architectural changes in `docs/`.
- Strict Rust — no `#[allow(unused)]` without justification.
- Write tests for new functionality; keep functions small and focused.
- Conventional commits (`type: description`), enforced by the `commit-msg` +
  `pre-push` hk hooks and CI via `convco` (escape hatch: `git commit --no-verify`;
  check a range manually with `mise run commit-check <base>..HEAD`).
- `thiserror` for error types, `anyhow` for application-level errors.

## Correctness (IMPORTANT!)

- Every component that talks to an external dependency is versioned and refuses a
  mismatch: the data protocol (`PROTOCOL_VERSION`), the `kmux-ffi` C ABI
  (`KMUX_FFI_ABI_VERSION`), the daemon↔worker contract (`kmux-worker-protocol`),
  and `kmux-ghostty-sys` (`EXPECTED_ABI_VERSION`).
- Every connecting party proves a cryptographic identity (issue #146): the daemon
  challenges each `Auth` with a random nonce and verifies the Ed25519 signature
  against the presented public key (its SHA-256 fingerprint is the `machine_id`)
  before trusting it, so no client can impersonate another. The shared token
  still gates access; identity is layered on top for attribution + management.
  See [docs/architecture-identity.md](docs/architecture-identity.md).
