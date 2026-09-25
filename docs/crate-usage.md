# Crate usage

This document is **normative**. It is the single place that records which crate
kmux uses for which job, where a version may be declared, how optional
dependencies are gated, and which layer is allowed to depend on what.

Read it before you add, remove, upgrade, or feature-gate a dependency. If the
tree needs to stop obeying a rule below, change the rule here in the same commit
that breaks it — a rule nobody updated is worse than no rule.

The per-`Cargo.toml` comments stay where they are: they explain *this pin, in
this manifest*, at the point you are editing. This document holds the rules and
the map. Where the two overlap, the manifest comment is the detail and this file
is the contract.

## Rules

**R1 — one declaration per external crate.** Every third-party version is
declared exactly once, in `[workspace.dependencies]` in the root `Cargo.toml`.
Members reference it as `<name>.workspace = true`. No member may write a version
string — not in `[dependencies]`, not in `[dev-dependencies]`, not in a
`[target.'cfg(...)'.dependencies]` block. Crate renames (`package = "..."`)
belong on the workspace entry too, so `adw` and `fontconfig-sys` resolve
identically everywhere.

**R2 — features widen at the member, never re-pin.** The workspace entry carries
the baseline feature set every consumer needs (`tokio`'s `full`, `clap`'s
`derive`). A member that needs more writes
`<name> = { workspace = true, features = [...] }`. A member that needs a
*narrower* build gets `default-features = false` on the workspace entry, with a
comment saying why (see the client-stack entries, which are lean by default so
GUI frontends do not inherit `quinn`/`rustls`/ssh).

**R3 — optional means feature-gated and documented.** An optional dependency is
reachable only through a named feature using `dep:` syntax, and that feature
carries a comment explaining what turns it on and what compiles out.
`kmux-protocol`, `kmux-render`, `kmux-connect`, and `kmuxd` are the reference
implementations of this pattern.

**R4 — no dead declarations.** A `[workspace.dependencies]` entry that no member
references is a defect, and so is a member declaration the crate's source never
names. Enforced by `mise run deps-audit` (cargo-machete) plus one grep; see
[Auditing](#auditing). Both are at zero.

**R5 — layering is a dependency rule, not a convention.** Nothing at or below
`kmux-app` may depend on a UI toolkit; `kmux-protocol` depends on no internal
crate; the graph stays acyclic. **Enforced** by
`xtask/tests/dependency_direction.rs`, which reads the `cargo metadata` resolve
graph and prints the shortest offending path on failure — this rule spent
months as a true statement nothing checked. It rides `mise run test`. See
[The workspace](#the-workspace),
[docs/quality-gates.md](quality-gates.md) and
[docs/architecture-frontend.md](architecture-frontend.md#layering).

**R6 — anything crossing a process or ABI boundary is versioned.** New
dependencies do not get to bypass the compatibility contracts listed under
*Correctness* in [AGENTS.md](../AGENTS.md) — the data protocol, the `kmux-ffi` C
ABI, the daemon↔worker contract, and `kmux-ghostty-sys`.

**R7 — a third-party licence or advisory is a gate, not a footnote.** Every
dependency's licence must be on the allow-list in `deny.toml`, and a RUSTSEC
advisory is a build failure until it is either fixed or written down there with
a reason. Enforced by `mise run deps-audit`.

## The workspace

Two stacks build on `kmux-protocol`: the client stack runs in the GUI process,
the server stack in `kmuxd` and its workers. Arrows point from a crate to what
it depends on.

**Client stack**

```
kmux-protocol      wire types, codec + framing, version compat. Pure data:
      ▲            no filesystem, no network, no crypto
      │
kmux-sys           XDG paths, machine identity, TLS/TOFU, transports
      ▲
      │
kmux-connect       QUIC / TCP+TLS / UDS / SSH, supervision, daemon lifecycle
      ▲
      │
kmux-client        SessionManager, CellGrid, key model
      ▲
      │
kmux-app           interaction policy, run_cli, FrontendDriver
      ▲
      ├─────────────────┬──────────────────┐
      │                 │                  │
   kmux-gtk         kmux-ffi             kmux
   GTK4 + adw       uniffi → Swift       entrypoint; execs a frontend
      │                 │
      └────────┬────────┘
               ▼
         kmux-render    wgpu cell-grid renderer; consumes
                        protocol/client/app types, depends on no UI toolkit
```

**Server stack**

```
                    kmux-protocol / kmux-sys
                              ▲
      ┌───────────────────────┼───────────────────────┐
      │                       │                       │
kmux-ghostty-sys          kmux-pty          kmux-worker-protocol
raw FFI to the Zig        PTY alloc,        daemon↔worker IPC contract
libghostty-vt wrapper     fd adoption               ▲
      ▲                       ▲                     │
      │                       │                     │
kmux-ghostty                  │                     │
safe façade                   │                     │
      ▲                       │                     │
      │                       │                     │
kmux-vt-core                  │                     │
backend, diff engine,         │                     │
scrollback mirror             │                     │
      ▲                       │                     │
      ├───────────────────────┴─────────────────────┤
      │                                             │
    kmuxd                                    kmux-vt-worker
    the daemon                               isolated per-pane VT subprocess
```

Two edges cross from the server stack into the client stack, and both are
deliberate: `kmuxd` depends on `kmux-client` for its `CellGrid` (reused as a
pane mirror for federation and process isolation) and, behind its `federation`
feature, on `kmux-connect` for outbound peer links. Neither client crate depends
on `kmuxd`, so no cycle exists.

### Internal crates

| Crate | Role | Internal deps | Design doc |
| --- | --- | --- | --- |
| `kmux-protocol` | Wire protocol: message types, codec + framing, version compat. Pure data — no filesystem, network or crypto | — | [protocol-versioning](architecture-protocol-versioning.md) |
| `kmux-sys` | The host-facing half: XDG path resolution, Ed25519 machine identity, TLS/TOFU material, the four transports, UDS peer credentials | `protocol` | [connection](connection.md), [identity](architecture-identity.md), [profile-isolation](profile-isolation.md) |
| `kmux-pty` | PTY allocation, fd adoption (`from_inherited`), reader/writer split, resize | — | [daemon-handoff](daemon-handoff.md) |
| `kmux-ghostty-sys` | Raw FFI to `libkmux_ghostty` (Zig wrapper over libghostty-vt). No Rust dependencies at all | — | [terminal-backend](terminal-backend.md) |
| `kmux-ghostty` | Safe façade over `kmux-ghostty-sys` | `-sys`, `protocol` | [terminal-backend](terminal-backend.md) |
| `kmux-vt-core` | Shared server-side VT pipeline: backend, diff engine, scrollback mirror | `protocol`, `ghostty` | [terminal-backend](terminal-backend.md), [process-isolation](architecture-process-isolation.md) |
| `kmux-worker-protocol` | Daemon↔worker IPC contract. Server-side only; no GUI frontend may depend on it | `protocol` | [process-isolation](architecture-process-isolation.md) |
| `kmux-vt-worker` | Isolated per-pane VT subprocess, so a terminal crash cannot take down `kmuxd` | `vt-core`, `worker-protocol`, `protocol`, `pty`, `ghostty-sys` | [process-isolation](architecture-process-isolation.md) |
| `kmuxd` | The daemon: sessions, panes, relay, persistence, federation | `pty`, `protocol`, `sys`, `client`, `worker-protocol`, `ghostty-sys`, `vt-core`, `connect`\* | [multiplexer](multiplexer.md), [daemon-lifecycle](daemon-lifecycle.md) |
| `kmux-connect` | Connection mechanism: QUIC/TCP+TLS/UDS/SSH transports, supervision, local daemon lifecycle | `protocol`, `sys` | [connection](connection.md), [federation](architecture-federation.md) |
| `kmux-client` | Client mechanism: `SessionManager`, `CellGrid`, key model | `protocol`, `sys`, `connect` | [frontend](architecture-frontend.md) |
| `kmux-app` | Interaction policy (toolkit-agnostic): modes/actions, `AppCore`, config + theme, `run_cli`, `FrontendDriver` | `client`, `protocol`, `sys` | [frontend](architecture-frontend.md) |
| `kmux-render` | Shared wgpu cell-grid renderer; consumed by both frontends | `protocol`, `client`, `app` | [render](architecture-render.md) |
| `kmux-gtk` | GTK4 + libadwaita frontend (Linux, also runnable on macOS) | `app`, `client`, `protocol`, `sys`, `render` | [frontend](architecture-frontend.md) |
| `kmux-ffi` | uniffi C ABI exposing `FrontendDriver` to the SwiftUI macOS app | `app`, `client`, `protocol`, `render` | [building-macos](building-macos.md) |
| `kmux` | Toolkit-free entrypoint: runs the shared CLI, else execs the platform frontend | `app`, `client`, `sys` | [frontend](architecture-frontend.md) |

\* optional, behind `kmuxd`'s `federation` feature.

### Feature gates

Features exist to keep binaries lean, never to express taste. Each one below
compiles out a dependency subtree.

| Crate | Feature | Default | What it turns on |
| --- | --- | --- | --- |
| `kmux-protocol` | `framing` | on | Length-prefixed async framing + per-frame zstd (`tokio`, `zstd`) |
| `kmux-sys` | `framing` | on | Forwards `kmux-protocol/framing`; everything here that moves bytes needs it |
| | `tls` | off | rustls stack, cert generation, TOFU store |
| | `quic` / `tcp-tls` / `uds` | off | One transport each; the first two imply `tls` |
| | `identity` | off | Ed25519 keypair persistence, sign, verify (`ring`, `sha2`) |
| | `client` / `server` | off | Gate `ClientTransport` / `Listener` impls |
| `kmux-connect` | `remote` | on\*\* | QUIC + TCP+TLS + SSH. Off leaves UDS-only |
| `kmux-client` | `remote` | on\*\* | Forwards `kmux-connect/remote` |
| `kmux-app` | `remote` | on\*\* | Forwards to `kmux-client`; gates SSH launch + `--test` probing |
| `kmuxd` | `federation` | on | Upstream links to remote daemons |
| `kmux-render` | `text` | off | Font rasterization + atlas (`swash`, `etagere`, `fontdb`). CPU-only |
| | `gpu` | off | The wgpu renderer; implies `text` |
| `kmux-gtk`, `kmux-ffi` | `gpu` | on | `kmux-render/gpu`. Runtime still defaults to CPU |
| `kmux-vt-core` | `test-util` | off | In-memory test doubles for downstream test builds |
| `kmux-pty` | `serde` | off | `serde` impls on the PTY types |

\*\* Default-on when the crate is built standalone, but the workspace entry sets
`default-features = false` so GUI frontends inherit the lean stack. Only `kmuxd`
opts back in. See [architecture-federation.md](architecture-federation.md).

## External crates

One crate per job. If you need something a listed crate already does, use the
listed crate.

### Errors

| Job | Crate | Rule |
| --- | --- | --- |
| Library error types | `thiserror` | Any crate exposing errors across its API boundary derives them |
| Application errors | `anyhow` | Binaries and orchestration code that only needs context, not matchable variants |

The split is by role, not by crate: `kmux-protocol` uses both — `thiserror` for
the error types it exports, `anyhow` for its own internal plumbing. Do not add a
third error crate, and do not hand-roll `impl Error` where `thiserror` fits.

### Diagnostics

| Job | Crate | Rule |
| --- | --- | --- |
| Structured logging | `tracing` | **The only logging API.** Every crate that logs uses it, so `kmux=*` env filters capture everything into the process log |
| Subscriber setup | `tracing-subscriber` | Only in crates that own a process entrypoint: `kmux-app`, `kmuxd`, `kmux-vt-worker` |
| The `log` facade | `log` | **Never call it.** Declared solely by `kmux-app` so `init_logging` can cap `tracing-log`'s bridge and silence uniffi 0.28's per-FFI-call scaffolding. See `crates/kmux-app/src/launch.rs` |

Log targets are crate-derived (`kmux_render`, `kmux::vt`, …), which is what makes
`kmux daemon logs` / `kmux client logs` filterable.

### Async

| Job | Crate | Rule |
| --- | --- | --- |
| Runtime, I/O, sync, time | `tokio` | The single runtime. The workspace entry enables `full`; do not re-enable it per member |
| Executor odds and ends | `futures` | Only `kmux-gtk`, for `block_on` where GTK's callback signatures are synchronous. Not a general-purpose import |

### Serialization and wire codecs

Four codecs, four non-overlapping jobs. Picking the wrong one is a wire-format
bug, so this table is the decision procedure:

| Codec | Used for | Consumers |
| --- | --- | --- |
| `rmp-serde` (MessagePack, named) | **The data-plane protocol.** The named-map encoding *is* the protocol | `kmux-protocol` |
| `postcard` | Worker IPC and on-disk daemon state — both internally versioned, neither crosses a version boundary | `kmux-worker-protocol`, `kmuxd` |
| `serde_json` | The JSON control/handoff RPC, SSH negotiation, `--format json` CLI output, JSONL metrics | `kmux-connect`, `kmux-client`, `kmux-app`, `kmuxd`. `kmux-protocol` defines the RPC types but only needs it as a dev-dependency, for its own round-trip tests |
| `toml` | Human-edited config and the TOFU store | `kmux-app`, `kmux-connect`, `kmuxd`, `kmux-sys` |

Supporting: `serde` (derives, everywhere), `serde_bytes` (`#[serde(with = ...)]`
on byte-slice fields so MessagePack emits a `bin` blob rather than an integer
array), `zstd` (per-frame compression behind `kmux-protocol/framing`; see
[compression.md](compression.md)).

### Transport and crypto

| Job | Crate | Notes |
| --- | --- | --- |
| QUIC | `quinn` | `kmux-sys`, `kmux-connect` (both optional), `kmuxd` |
| TLS | `rustls` + `tokio-rustls` | `ring` backend, no `aws-lc`. `default-features = false` keeps the feature set explicit |
| Root certs / PEM / cert gen | `rustls-native-certs`, `rustls-pemfile`, `rcgen` | `kmux-sys`'s `tls` feature only — do not re-declare downstream |
| Ed25519 identity | `ring` | `kmux-sys`'s `identity` feature. Already in-tree via rustls; pinned to the resolved version |
| Hashing | `sha2` | Machine-ID fingerprints (`identity`) and TOFU certificate fingerprints (`tls`) |
| Random | `rand` | Session names, instance IDs, and the shared auth token (`kmuxd`, `kmux-client`) |

Identity material does **not** go through `rand`: challenge nonces and Ed25519
keys use `ring::rand::SystemRandom` in `crates/kmux-sys/src/identity.rs`.
Keep it that way — the signature scheme and its RNG should come from one crate.

### CLI and terminal output

| Job | Crate | Notes |
| --- | --- | --- |
| Argument parsing | `clap` | `derive` + `unstable-ext`. `kmux-app` owns the shared `Cli`; `kmux` re-parses it on macOS to build the `kmux://` launch URL |
| Shell completion | `clap_complete` | `unstable-dynamic`. Tilde-pinned in lockstep with clap — see [Version pins](#version-pins) |
| Table output | `tabled` | Every CLI table (`kmux ls`, `ps`, `clients`, `status`), all defined as `#[derive(Tabled)]` rows in `crates/kmux-app/src/subcommands/render.rs`. Add a row type there rather than hand-formatting columns |
| Base64 | `base64` | Decoding OSC 52 clipboard payloads in `kmux-app` |

### Platform and process

| Job | Crate | Notes |
| --- | --- | --- |
| POSIX syscalls | `nix` | fds, sockets, signals, termios, `SCM_RIGHTS` fd passing. The only libc surface — do not add `libc` directly |
| Daemonization | `daemonize` | `kmuxd` only |
| Process stats | `sysinfo` | Per-pane CPU/memory for [process-overview](architecture-process-overview.md). `default-features = false` drops probes we never query |
| Compile-time assertions | `static_assertions` | `kmux-ghostty`, to hold `Send`/`Sync` claims over the FFI boundary |
| Bit flags | `bitflags` | Key modifiers and terminal mode sets, shared across the protocol boundary |
| Lock-free handoff | `arc-swap` | The off-UI-thread grid publish double-buffer (issue #182) |

### Rendering

`wgpu` (GPU), `swash` + `etagere` + `fontdb` (glyph rasterization, atlas packing,
font lookup), `bytemuck` (vertex POD casts), `pollster` (blocking on wgpu's async
init). All optional, all confined to `kmux-render` behind its `text`/`gpu` tiers.

The GTK stack — `gtk4`, `adw` (libadwaita), `pangocairo`, `fontconfig-sys` — is
confined to `kmux-gtk` and target-gated to Linux + macOS, where the system
packages exist; elsewhere `kmux-gtk` compiles to a stub binary. Under R5 these
may never appear at or below `kmux-app`, which is checkable:
`cargo tree -p kmux-app` shows no `gtk4`.

### FFI

`uniffi` generates the Swift bindings, and only `kmux-ffi` declares it. Its `cli`
feature backs the in-tree `uniffi-bindgen` binary. The C ABI it produces is
versioned by `KMUX_FFI_ABI_VERSION` under R6.

### Dev-only

`tempfile` (isolated state dirs in tests) and `proptest` (property tests for the
grid-publish handoff). Both belong in `[dev-dependencies]`. Do not add a
dev-dependency a regular dependency already provides — regular deps are
available to tests, and a duplicate entry is an R4 violation.

## Version pins

Default is caret (`"1"`, `"0.31"`) — take compatible upgrades. Two crates are
deliberately stricter:

| Crate | Pin | Why |
| --- | --- | --- |
| `rmp-serde` | `=1.3.1` | The named-map encoding *is* the data-plane wire format. An encoder change is a protocol change, not a dependency update. Bump deliberately and re-check the fixture tests in `kmux-protocol`'s codec module |
| `clap_complete` | `~4.6` | `unstable-dynamic` and clap's `unstable-ext` track clap's minor version. Keep it in lockstep with clap 4.6.x. See [shell-completion.md](shell-completion.md) |

Toolchain-sensitive versions (the wgpu stack) are verified to resolve on the
pinned toolchain in `rust-toolchain.toml`. Releases do not touch
`[workspace.dependencies]` — see [releasing.md](releasing.md).

## Adding or removing a dependency

1. Check this document first. If an existing crate does the job, use it.
2. Add the version to `[workspace.dependencies]` with a comment saying what it
   is for (R1). Reference it from the member as `<name>.workspace = true`.
3. If it is only needed on some builds, make it `optional` and put it behind a
   named, commented feature (R3). Confirm the lean path still compiles:
   `mise run build-no-gpu`.
4. If it lands at or below `kmux-app`, confirm it drags in no UI toolkit (R5).
5. When removing code, remove the declaration in the same commit, and drop the
   workspace entry if that was the last consumer (R4).
6. Run the audit below, then `mise run clippy` and `mise run test`.

### Auditing

`mise run deps-audit` is the check that runs in CI. It is two tools:

* **cargo-deny** (`deny.toml`) — third-party licence compatibility and RUSTSEC
  advisories. The dual `AGPL-3.0-only OR LicenseRef-Commercial` licence makes
  the first a commercial requirement rather than hygiene, and a daemon that
  speaks QUIC and TLS to the network has no business carrying an unchecked
  rustls/ring/quinn tree. The first run found three live vulnerabilities.
* **cargo-machete** — a declared dependency whose crate never uses it. This
  replaces the R4b grep below, which was a *name* check: it proved a crate was
  mentioned somewhere, so a dependency named only in a comment passed. Its
  first run found `kmux-ghostty` declared by `kmuxd` and used by nothing.
  cargo-machete reads `use` statements, so a dependency used any other way —
  `#[serde(with = "…")]`, or purely for its build-script metadata — needs a
  `[package.metadata.cargo-machete] ignored = […]` entry in that manifest with
  a comment saying why. Three exist today and each one says which.

Two checks have no tool and are still greps, run from the repository root, both
expected to print nothing:

```sh
# R1 — a member declaring an inline version instead of `.workspace = true`.
# Covers [dependencies], [dev-dependencies] and [target.'cfg(...)'.dependencies].
awk '/^\[/{f=($0 ~ /dependencies\]/)} f && /^[a-zA-Z0-9_-]+ *=/ && !/workspace/ \
  {print FILENAME":"FNR": "$0}' crates/*/Cargo.toml

# R4a — a [workspace.dependencies] entry no member references.
awk '/^\[workspace.dependencies\]/{f=1;next} /^\[/{f=0} f && /^[a-zA-Z0-9_-]+ *=/ \
  {sub(/ *=.*/,"");print}' Cargo.toml |
  while read -r d; do
    grep -qE "^ *$d(\.workspace)? *=" crates/*/Cargo.toml || echo "dead: $d"
  done
```

`cargo tree -p <crate>` and `cargo tree -i <dep>` answer "who pulls this in?"
when a lockfile diff is larger than expected.

## Known exceptions

- **`kmux-ghostty-sys` declares no Rust dependencies at all.** It links a Zig
  static library and is versioned by `EXPECTED_ABI_VERSION` instead. See
  [terminal-backend.md](terminal-backend.md).
- **`zstd` is declared twice by `kmux-protocol`** — optional under `framing`,
  and again as a plain dev-dependency for the `compression_bench` example, which
  sweeps zstd levels offline. Cargo cannot express an optional dev-dependency,
  so this is the intended shape, not an R4 violation.
- **The GTK stack is target-gated**, so `cargo metadata` reports it on every
  platform while only Linux and macOS actually build it.
