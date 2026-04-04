# kmux

A terminal multiplexer and session manager with remote desktop capabilities.
Connect to persistent terminal sessions over encrypted QUIC connections
from a native desktop GUI.

## Features

TODO

- [x] Linux and macOS support (no Windows for now)

## Architecture

kmux uses a server/client split:

| Crate                                   | Role                                                                   |
| --------------------------------------- | ---------------------------------------------------------------------- |
| [`kmux-pty`](crates/kmux-pty)           | Async PTY lifecycle library (spawn, I/O, resize, shutdown)             |
| [`kmux-protocol`](crates/kmux-protocol) | Shared wire protocol with postcard serialization                       |
| [`kmux-server`](crates/kmux-server)     | Background daemon — manages PTY sessions, accepts QUIC+TLS connections |
| [`kmux`](crates/kmux)                   | Desktop GUI client built on [iced](https://github.com/iced-rs/iced)    |

```
kmux-server  ->  kmux-pty
kmux-server  ->  kmux-protocol
kmux         ->  kmux-protocol
```

The client talks to the server over QUIC+TLS. It does not depend on `kmux-pty`
directly.

## Prerequisites

- Rust toolchain (edition 2024) via [rustup](https://rustup.rs)

## Quick start

Start the server with a self-signed certificate:

```bash
$ cargo run -p kmux-server -- --self-signed
```

The server prints a shared auth token on startup. Connect the GUI client:

```bash
$ cargo run -p kmux
```

By default, the server binds to `0.0.0.0:8443`.

## Server options

```
--bind <addr>       Bind address (default: 0.0.0.0)
--port <port>       Bind port (default: 8443)
--cert <path>       TLS certificate (PEM)
--key <path>        TLS private key (PEM)
--self-signed       Generate a self-signed certificate
```

Provide `--cert` and `--key` for production use. Use `--self-signed` for
local development.

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
