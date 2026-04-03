# smux

A terminal multiplexer and session manager with remote desktop capabilities.
Connect to persistent terminal sessions over encrypted QUIC connections
from a native desktop GUI.

## Architecture

smux uses a server/client split:

| Crate | Role |
|-------|------|
| [`smux-pty`](crates/smux-pty) | Async PTY lifecycle library (spawn, I/O, resize, shutdown) |
| [`smux-protocol`](crates/smux-protocol) | Shared wire protocol with postcard serialization |
| [`smux-server`](crates/smux-server) | Background daemon — manages PTY sessions, accepts QUIC+TLS connections |
| [`smux`](crates/smux) | Desktop GUI client built on [iced](https://github.com/iced-rs/iced) |

```
smux-server  ->  smux-pty
smux-server  ->  smux-protocol
smux         ->  smux-protocol
```

The client talks to the server over QUIC+TLS. It does not depend on `smux-pty`
directly.

## Prerequisites

- Rust toolchain (edition 2024) via [rustup](https://rustup.rs)

## Quick start

Start the server with a self-signed certificate:

```bash
$ cargo run -p smux-server -- --self-signed
```

The server prints a shared auth token on startup. Connect the GUI client:

```bash
$ cargo run -p smux
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
