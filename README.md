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
| [`kmux-gtk`](crates/kmux-gtk)           | GTK4 GUI frontend (Linux), **primary**; ships the `kmux` binary            |
| [`kmux-tui`](crates/kmux-tui)           | TUI frontend (ratatui/crossterm), *deprecated*; ships the `kmux-tui` binary |

```
kmuxd     ->  kmux-pty,  kmux-protocol
kmux-client ->  kmux-protocol
kmux-app  ->  kmux-client, kmux-protocol
kmux-tui  ->  kmux-app          (ratatui/crossterm)
kmux-gtk  ->  kmux-app          (gtk4)
```

The client is layered so one toolkit-agnostic core (`kmux-app`'s `AppCore`)
drives both frontends. See [docs/architecture-frontend.md](docs/architecture-frontend.md).

The client connects to the server over QUIC (preferred), TCP+TLS (fallback), or
UDS (local). See [docs/connection.md](docs/connection.md) for a full description
of the two-phase connection model, transport selection, and `kmuxd.toml`
configuration.

## Terminology

| Term | Meaning |
| --- | --- |
| **Daemon** (`kmuxd`) | Background server that owns PTYs, runs the VT emulator, persists state, and serves clients. |
| **Client** (`kmux`) | The GTK GUI front-end (Linux, primary) that connects to a daemon and renders the terminal grid. `kmux-tui` is the deprecated terminal front-end, kept for SSH/headless use. |
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
| **Command mode** | `/`-prefixed floating overlay (activated with **Ctrl+G** then `/`) for running TUI commands such as switching sessions or attaching to servers. |
| **Terminal backend** | The VT emulator running inside the daemon (currently [`libghostty-vt`](vendor/ghostty)); clients receive resolved cell data, never raw escape sequences. |

## Prerequisites

- Rust toolchain (edition 2024) via [rustup](https://rustup.rs)
- Zig `0.15.2`, managed via [mise](https://mise.jdx.dev) (`mise install` reads
  `mise.toml`); required to build the bundled libghostty-vt wrapper.
- Ghostty sources as a git submodule: after cloning, run
  `git submodule update --init` once to populate `vendor/ghostty/`.
- **GTK4 development libraries** — required only to build the `kmux` GUI binary
  (`kmux-gtk`). The `kmux-tui` and `kmuxd` binaries do not need them.
  - Fedora: `sudo dnf install gtk4-devel`
  - Debian / Ubuntu: `sudo apt install libgtk-4-dev`
  - Arch: `sudo pacman -S gtk4`
  - macOS: `brew install gtk4`
  - If another `pkg-config` (e.g. a Homebrew/linuxbrew one) shadows the system
    one in `PATH`, GTK4 resolution fails on transitive `.pc` files. Point cargo
    at the system pkg-config for any build that includes `kmux-gtk`:
    `PKG_CONFIG=/usr/bin/pkg-config cargo run --bin kmux`. See
    [docs/architecture-frontend.md](docs/architecture-frontend.md#building-kmux-gtk-and-the-system-pkg-config).

## Quick start

Start the server with a self-signed certificate:

```bash
$ cargo run -p kmuxd -- --self-signed
```

The server prints a shared auth token on startup. Connect a client — the GTK
GUI (`kmux`, Linux) or the terminal UI (`kmux-tui`):

```bash
$ cargo run --bin kmux    # GUI  (kmux-gtk)   — needs system GTK4 dev libs
$ cargo run -p kmux-tui   # TUI  (binary: kmux-tui)
```

> If `cargo run --bin kmux` fails with a `pkg-config` error about
> `graphene-gobject-1.0` (or similar) not being found, your `PATH` has a
> non-system `pkg-config` shadowing `/usr/bin/pkg-config`. Re-run as
> `PKG_CONFIG=/usr/bin/pkg-config cargo run --bin kmux`. See
> [Prerequisites](#prerequisites).

By default, the server binds to `0.0.0.0:8443`.

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

For development with a self-signed certificate, the legacy flag still works:

```bash
$ cargo run -p kmuxd -- --self-signed
```

## Development

```bash
cargo build --workspace    # build everything
cargo test --workspace     # run all tests
cargo fmt --all            # format
cargo clippy --workspace   # lint
```

Git hooks (via [lefthook](https://github.com/evilmartians/lefthook)) run
format and lint checks on commit, plus tests on push.

## Contributing

Run `cargo test --workspace` and `cargo clippy --workspace` before submitting
a PR. The project uses conventional commits (`type: description`).

## License

All rights reserved.
