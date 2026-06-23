# kmux

A terminal multiplexer and session manager with remote desktop capabilities.
Connect to persistent terminal sessions over encrypted QUIC connections
from a native desktop GUI.

## Features

TODO

- [x] Linux and macOS support (no Windows for now)
- [x] `--dry-run` / `--test` connection diagnostics — trace the real bootstrap, verify with ping, exit. See [docs/connection.md](docs/connection.md#dry-run-diagnostics---dry-run---test).

## Architecture

kmux uses a server/client split:

| Crate                                   | Role                                                                       |
| --------------------------------------- | -------------------------------------------------------------------------- |
| [`kmux-pty`](crates/kmux-pty)           | Async PTY lifecycle library (spawn, I/O, resize, shutdown)                 |
| [`kmux-protocol`](crates/kmux-protocol) | Shared wire protocol, transport traits, TOFU TLS, endpoint URL parser      |
| [`kmuxd`](crates/kmuxd)                 | Background daemon — manages PTY sessions, multi-transport listener         |
| [`kmux-client`](crates/kmux-client)     | Client mechanism — session manager, transports/bootstrap, terminal grid    |
| [`kmux-app`](crates/kmux-app)           | Toolkit-agnostic interaction layer — `AppCore`, command palette, CLI       |
| [`kmux`](crates/kmux)                   | Entrypoint binary (`kmux`) — CLI + execs the platform desktop app          |
| [`kmux-gtk`](crates/kmux-gtk)           | GTK4 GUI frontend (Linux **primary**, also macOS); ships the `kmux-gtk` binary |

```
kmuxd     ->  kmux-pty,  kmux-protocol
kmux-client ->  kmux-protocol
kmux-app  ->  kmux-client, kmux-protocol
kmux      ->  kmux-app          (entrypoint; execs kmux-gtk / kmux.app)
kmux-gtk  ->  kmux-app          (gtk4; Linux + macOS)
```

The client is layered so one toolkit-agnostic core (`kmux-app`'s `AppCore`)
drives the frontends, and the toolkit-free `kmux` entrypoint launches the
platform desktop app. See [docs/architecture-frontend.md](docs/architecture-frontend.md).

The client connects to the server over QUIC (preferred), TCP+TLS (fallback), or
UDS (local). See [docs/connection.md](docs/connection.md) for a full description
of the two-phase connection model, transport selection, and `kmuxd.toml`
configuration.

## Terminology

| Term | Meaning |
| --- | --- |
| **Daemon** (`kmuxd`) | Background server that owns PTYs, runs the VT emulator, persists state, and serves clients. |
| **Client** (`kmux`) | The entrypoint command: it offers the CLI and opens the platform desktop app — `kmux-gtk` (GTK GUI; Linux default, also macOS) or `kmux-swift` (native macOS app). |
| **Session** | Top-level container holding one or more panes; identified by a stable word ID and survives client disconnects. |
| **Pane** | A single shell process inside a session, backed by its own PTY and scrollback. |
| **PTY** | Pseudo-terminal — the OS device pair that backs every running shell. |
| **Word ID** | Human-readable identifier auto-assigned to each session (e.g. `eagle`, `hippo`); persists across daemon restarts. |
| **Scrollback** | Server-side ring buffer of past terminal output (default 50,000 lines) that clients scroll into on demand. |
| **Checkpoint** | Periodic on-disk snapshot of sessions, panes, and scrollback so state survives daemon restart. |
| **Transport** | Network channel between client and daemon: QUIC (preferred), TCP+TLS (fallback), or UDS (local). |
| **Endpoint** | A single network address (host:port or UDS path) advertised by the daemon for a given transport. |
| **Audience** | Visibility filter on an endpoint (`any`, `lan`, `local`, `ssh-only`) that controls which callers see it. |
| **Bootstrap** | Phase A of connecting: authenticate and fetch the audience-filtered endpoint catalog (via SSH, QUIC, TCP+TLS, or UDS). |
| **Transport upgrade** | Phase B of connecting: continuously score available transports by RTT/reliability and switch to the best one. |
| **TOFU TLS** | Trust-On-First-Use certificate pinning — self-signed or private-CA certs are accepted on first connect and pinned for later connects. |
| **Command mode** | `/`-prefixed floating overlay (activated with **Ctrl+G** then `/`) for running commands such as switching sessions or attaching to servers. |
| **Terminal backend** | The VT emulator running inside the daemon (currently [`libghostty-vt`](vendor/ghostty)); clients receive resolved cell data, never raw escape sequences. |

## Prerequisites

- Rust toolchain (edition 2024) via [rustup](https://rustup.rs)
- Zig `0.15.2`, managed via [mise](https://mise.jdx.dev) (`mise install` reads
  `mise.toml`); required to build the bundled libghostty-vt wrapper.
  - The `kmux-ghostty-sys` build pins zig to exactly `0.15.2` and aborts on any
    other version. If mise is **not** activated in your shell (no
    `mise activate` in your shell rc, so no shims on `PATH`), a different `zig`
    (e.g. a Homebrew one) may be used instead of the pinned one and builds may fail with a version
    mismatch. The `mise` tasks run under mise's activated toolchain, so the
    pinned zig is always on `PATH` — `mise run install` / `mise run build` work
    regardless. For a raw `cargo build`/`cargo install`, either activate mise,
    run it under `mise exec -- cargo …`, or pass `ZIG="$(mise which zig)" cargo …`.
- Ghostty sources as a git submodule: after cloning, run
  `git submodule update --init` once to populate `vendor/ghostty/`.
- **GTK4 development libraries** — required only to build the GTK GUI binary
  (`kmux-gtk`). The `kmux` entrypoint and `kmuxd` binaries do not need them.
  - Fedora: `sudo dnf install gtk4-devel libadwaita-devel`
  - Debian / Ubuntu: `sudo apt install libgtk-4-dev libadwaita-1-dev`
  - Arch: `sudo pacman -S gtk4 libadwaita`
  - macOS: `brew install gtk4 libadwaita`
  - If another `pkg-config` (e.g. a Homebrew/linuxbrew one) shadows the system
    one in `PATH`, GTK4 resolution fails on transitive `.pc` files. Point cargo
    at the system pkg-config for any build that includes `kmux-gtk`:
    `PKG_CONFIG=/usr/bin/pkg-config cargo run -p kmux-gtk`. See
    [docs/architecture-frontend.md](docs/architecture-frontend.md#building-and-running-kmux-gtk).

## Quick start

Start the server:

```bash
$ cargo run -p kmuxd
```

With no certificate configured, the server generates an in-memory self-signed
certificate — the default for this kind of software, so no flag is needed.
The server prints a shared auth token on startup. Connect with the `./kmux` dev
entrypoint — it mirrors the installed binary, building the debug binaries from
this checkout and pinning the debug daemon:

```bash
$ ./kmux                   # launch the GUI (native Swift app on macOS, kmux-gtk on Linux)
$ ./kmux daemon status     # run a CLI subcommand (no GUI)
```

`./kmux` forwards to `mise run dev`. To run a frontend crate directly instead,
`cargo run -p kmux-gtk` opens the GTK GUI (needs GTK4 dev libs).

> If a `kmux-gtk` build fails with a `pkg-config` error about
> `graphene-gobject-1.0` (or similar) not being found, your `PATH` has a
> non-system `pkg-config` shadowing `/usr/bin/pkg-config`. Re-run as
> `PKG_CONFIG=/usr/bin/pkg-config cargo run -p kmux-gtk`. See
> [Prerequisites](#prerequisites).

By default, the server binds to `0.0.0.0:8443`.

To enable dynamic tab-completion for `kmux` (subcommands, flags, themes,
`hosts.toml` aliases, and live sessions), add one line to your shell config — see
[docs/shell-completion.md](docs/shell-completion.md).

## Server configuration

Configure `kmuxd` with a TOML file (see [docs/connection.md](docs/connection.md)
for the full schema):

```
$XDG_CONFIG_HOME/kmuxd/kmuxd.toml    (user)
/etc/kmuxd/kmuxd.toml                (system)
```

Use `--config <path>` to specify a custom path, or `print-config` to dump
effective defaults:

```bash
$ cargo run -p kmuxd -- print-config
```

To serve a custom certificate, set `[tls] cert` and `[tls] key` in `kmuxd.toml`
(or pass `--cert`/`--key`); otherwise a self-signed certificate is generated.

## Development

Tasks run through [mise](https://mise.jdx.dev) (`mise tasks` lists them all):

```bash
mise run build    # cargo build
mise run test     # cargo test --workspace
mise run fmt      # cargo fmt --all
mise run clippy   # cargo clippy (warnings denied)
./kmux            # build + launch the GUI (mirrors the installed binary)
```

Git hooks are managed by [hk](https://hk.jdx.dev) (pinned in `mise.toml`):
format + lint auto-fix on commit (the fixes are auto-staged for you), and
format/lint/tests on push. `mise install` installs hk and wires the hooks into
git automatically; if the hooks aren't active, run `mise run setup`.

## Contributing

Run `mise run test` and `mise run clippy` before submitting a PR (the hk
pre-push hook runs these for you). The project uses conventional commits
(`type: description`).

## License

All rights reserved.
