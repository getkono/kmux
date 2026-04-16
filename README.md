# kmux

A terminal multiplexer and session manager with remote desktop capabilities.
Connect to persistent terminal sessions over encrypted QUIC connections
from a native desktop GUI.

## Features

TODO

- [x] Linux and macOS support (no Windows for now)

## Architecture

kmux uses a server/client split:

| Crate                                   | Role                                                                       |
| --------------------------------------- | -------------------------------------------------------------------------- |
| [`kmux-pty`](crates/kmux-pty)           | Async PTY lifecycle library (spawn, I/O, resize, shutdown)                 |
| [`kmux-protocol`](crates/kmux-protocol) | Shared wire protocol, transport traits, TOFU TLS, endpoint URL parser      |
| [`kmuxd`](crates/kmuxd)                 | Background daemon — manages PTY sessions, multi-transport listener         |
| [`kmux`](crates/kmux)                   | TUI client — connects over QUIC, TCP+TLS, or UDS with automatic fallback   |

```
kmuxd  ->  kmux-pty
kmuxd  ->  kmux-protocol
kmux   ->  kmux-protocol
```

The client connects to the server over QUIC (preferred), TCP+TLS (fallback), or
UDS (local). See [docs/connection.md](docs/connection.md) for a full description
of the two-phase connection model, transport selection, and `kmuxd.toml`
configuration.

## Prerequisites

- Rust toolchain (edition 2024) via [rustup](https://rustup.rs)

## Quick start

Start the server with a self-signed certificate:

```bash
$ cargo run -p kmuxd -- --self-signed
```

The server prints a shared auth token on startup. Connect the GUI client:

```bash
$ cargo run -p kmux
```

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
