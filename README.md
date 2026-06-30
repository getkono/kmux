# kmux

A terminal multiplexer and session manager with remote desktop capabilities.
Connect to persistent terminal sessions over encrypted QUIC connections
from a native desktop GUI.

## Features

TODO

- [x] Linux and macOS support (no Windows for now)
- [x] `--dry-run` / `--test` connection diagnostics — trace the real bootstrap, verify with ping, exit. See [docs/connection.md](docs/connection.md#dry-run-diagnostics---dry-run---test).

## Install

kmux has two halves: a background **daemon** (`kmuxd`) that owns your terminal
sessions, and a **client** that connects to it. A common setup runs the daemon on
a server and a GUI client on your desktop, connected over encrypted QUIC — but
both can also live on one machine.

Pick the **full** install (desktop GUI + daemon) for a workstation, or the
**headless** install (daemon + CLI only) for a server. For the complete
reference — every distro, manual download + checksum verification, install
layout, and offline installs — see [docs/installation.md](docs/installation.md).

### Full (desktop GUI + daemon)

**macOS** — the signed, notarized app:

```bash
brew install --cask getkono/tap/kmux
```

…or download the `.dmg` from the [latest release](https://github.com/getkono/kmux/releases/latest).

**Linux** — a native package for your distro (the GUI's GTK4 + libadwaita runtime
deps are pulled in automatically):

| Distro | Command |
| --- | --- |
| Debian / Ubuntu | `sudo apt install ./kmux_<ver>_<arch>.deb` |
| Fedora / RHEL | `sudo dnf install ./kmux-<ver>.<arch>.rpm` |
| Arch (AUR) | `paru -S kmux-bin` |
| Flatpak | `flatpak install ./kmux-<ver>-x86_64.flatpak` |

Download the `.deb` / `.rpm` / `.flatpak` from the [latest release](https://github.com/getkono/kmux/releases/latest).
Or use the universal installer (works on Linux and macOS; on macOS it installs the
CLI + daemon and points you at the GUI app):

```bash
curl -fsSL https://raw.githubusercontent.com/getkono/kmux/master/install.sh | sh
```

The `install.sh` GUI install needs GTK4 + libadwaita already present
(`apt install libgtk-4-1 libadwaita-1-0`, `dnf install gtk4 libadwaita`,
`pacman -S gtk4 libadwaita`); the native packages declare them for you.

### Headless (server / daemon-only)

No GUI, no GTK dependency — just `kmuxd` and the `kmux` CLI:

```bash
curl -fsSL https://raw.githubusercontent.com/getkono/kmux/master/install.sh | sh -s -- --headless
```

…or a native package / the Homebrew formula:

| Method | Command |
| --- | --- |
| Debian / Ubuntu | `sudo apt install ./kmux-headless_<ver>_<arch>.deb` |
| Fedora / RHEL | `sudo dnf install ./kmux-headless-<ver>.<arch>.rpm` |
| Homebrew (incl. Linuxbrew) | `brew install getkono/tap/kmux` |

Run `kmuxd` on the server, then connect from a full GUI client on your desktop
(`kmux --server <host>`). See [Configuration](#configuration) for binding,
certificates, and the auth token.

### Verify, upgrade, uninstall

```bash
kmux --version            # check the install
kmux                      # launch the GUI / connect (full install)
```

Re-run `install.sh` to upgrade; `install.sh --uninstall` to remove (your config +
session state are left intact). Native packages upgrade through your package
manager. If `~/.local/bin` isn't on your `PATH`, the installer prints the line to
add.

To enable dynamic tab-completion for `kmux` (subcommands, flags, themes,
`hosts.toml` aliases, and live sessions), add one line to your shell config — see
[docs/shell-completion.md](docs/shell-completion.md).

To build from source instead, see [Building from source](#building-from-source).

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

## Configuration

Configure `kmuxd` with a TOML file (see [docs/connection.md](docs/connection.md)
for the full schema):

```
$XDG_CONFIG_HOME/kmuxd/kmuxd.toml    (user)
/etc/kmuxd/kmuxd.toml                (system)
```

Use `--config <path>` to specify a custom path, or `print-config` to dump
effective defaults:

```bash
$ kmuxd print-config        # installed; from a source checkout: cargo run -p kmuxd -- print-config
```

To serve a custom certificate, set `[tls] cert` and `[tls] key` in `kmuxd.toml`
(or pass `--cert`/`--key`); otherwise a self-signed certificate is generated.
By default, the server binds to `0.0.0.0:8443` and prints a shared auth token on
startup that clients present when connecting.

## Building from source

Build prerequisites (the installed packages above need none of these — they are
only for building kmux yourself):

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
- Ghostty sources as a git submodule, populating `vendor/ghostty/`. `mise install`
  (or `mise run setup`) initializes it for you; to do it by hand, run
  `git submodule update --init --recursive` at the repo root.
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
> `PKG_CONFIG=/usr/bin/pkg-config cargo run -p kmux-gtk`.

To install a from-source build the way the packages do (the `kmux` entrypoint +
`kmuxd` to `~/.cargo/bin`, and the GUI as `~/Applications/kmux.app` on macOS or a
`.desktop` entry on Linux), run `mise run install`. See
[docs/building-macos.md](docs/building-macos.md) for the macOS app build.

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
format/lint/tests on push. `mise install` installs hk, wires the hooks into git,
and initializes the `vendor/ghostty/` submodule automatically; if any of that
didn't take (e.g. no network on first run), run `mise run setup`.

Releases are cut with `mise run release <ver>`; the tag-triggered workflow builds
the tarballs, native packages, and the signed macOS app. See
[docs/releasing.md](docs/releasing.md) and [packaging/](packaging).

## Contributing

Run `mise run test` and `mise run clippy` before submitting a PR (the hk
pre-push hook runs these for you). The project uses conventional commits
(`type: description`).

Because kmux is dual-licensed (see [License](#license)), external contributors
must sign the [Contributor License Agreement](CLA.md) before their pull request
can be merged. A bot comments on your first PR with a one-line phrase to reply
with; you only sign once. The CLA grants Kono the right to include your
contribution under both the AGPL and the commercial license — it does not take
away any of your own rights to your work.

## License

Copyright (C) 2026 Kono.

kmux is dual-licensed under the **GNU Affero General Public License v3.0**
([`LICENSE`](LICENSE)) **OR** a commercial license:

- **Open source (default):** you may use, modify, and redistribute kmux under the
  terms of the [AGPL-3.0](LICENSE). The community is the focus, and the AGPL keeps
  kmux and its derivatives open — including over a network.
- **Commercial:** organizations that want to use or modify kmux internally without
  the AGPL's network-copyleft obligations can obtain a commercial license. No
  commercial terms are published yet; the option is here to keep the door open.
  To inquire, [open an issue](https://github.com/getkono/kmux/issues).

The intent is to keep kmux genuinely open-source while leaving room for commercial
use. All copyrights are retained by Kono.

Unless you explicitly state otherwise, any contribution you submit for inclusion
in kmux (as defined in the AGPL-3.0) shall be dual-licensed as above, under the
terms of the [Contributor License Agreement](CLA.md), without any additional terms
or conditions.
