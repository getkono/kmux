# smux

A terminal multiplexer / session manager with remote desktop capabilities.

## Tech Stack

- **Runtime:** Rust
- **Language:** Rust
- **Formatter:** rustfmt
- **Linter:** Clippy

## Project Structure

```
smux/
├── .github/
│   └── workflows/
│       └── ci.yml
├── crates/
│   ├── smux/                   # Core async PTY library
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs
│   │       ├── error.rs
│   │       ├── events.rs
│   │       ├── expect.rs
│   │       ├── io.rs
│   │       ├── mock.rs
│   │       ├── oneshot.rs
│   │       ├── platform.rs
│   │       ├── probe.rs
│   │       ├── process.rs
│   │       ├── pty.rs
│   │       ├── registry.rs
│   │       ├── resize.rs
│   │       ├── session.rs
│   │       ├── shell.rs
│   │       ├── shutdown.rs
│   │       └── timeout.rs
│   ├── smux-protocol/          # Shared wire protocol (MessagePack)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── messages.rs
│   │       └── frame.rs
│   ├── smux-server/            # Remote PTY daemon (TLS WebSocket server)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── app.rs
│   │       ├── auth.rs
│   │       ├── connection.rs
│   │       ├── relay.rs
│   │       └── tls.rs
│   └── smux-client/            # Desktop GUI (iced + iced_term)
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── app.rs
│           ├── connect.rs
│           ├── terminal_view.rs
│           ├── session_bar.rs
│           └── theme.rs
├── .gitignore
├── AGENTS.md
├── Cargo.toml                  # [workspace] manifest
├── CLAUDE.md -> AGENTS.md
└── lefthook.yml
```

### Dependency Graph

```
smux-server  ──→ smux (library)
smux-server  ──→ smux-protocol
smux-client  ──→ smux-protocol
smux-client     (does NOT depend on smux — talks to server only)
```

## Development

### Setup

```bash
# Rust toolchain (via rustup)
rustup toolchain install stable
```

### Run (server)

```bash
cargo run -p smux-server -- --self-signed
```

### Run (client)

```bash
cargo run -p smux-client
```

### Test

```bash
cargo test --workspace
```

### Format

```bash
cargo fmt --all
```

### Lint

```bash
cargo clippy --workspace
```

### Lint Fix

```bash
cargo clippy --workspace --fix
```

## Conventions

- Use strict Rust — no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors

## Architecture

smux is a terminal multiplexer / session manager. The architecture is a
server/client split:

- **smux** (library): async PTY lifecycle management (spawn, I/O, resize, shutdown, events)
- **smux-protocol**: shared wire protocol types, MessagePack serialization
- **smux-server**: background daemon; manages PTY sessions, accepts WebSocket+TLS
  connections from clients, relays PTY I/O
- **smux-client**: iced-based desktop GUI; connects to a remote smux-server,
  renders terminal output via iced_term (alacritty backend)

### Network Protocol

- Transport: WebSocket over TLS
- Encoding: MessagePack (via rmp-serde)
- Auth: shared token (hex-encoded 32 random bytes, printed at server startup)
- Each WS binary frame = one MessagePack-encoded `ClientMessage` or `ServerMessage`

### PTY Output Relay

```
PTY child stdout → PtyMasterIo (AsyncRead)
→ relay task (session_read_loop)
→ broadcast::Sender<Vec<u8>>   (one per session)
→ broadcast::Receiver          (one per attached client)
→ output-forward task          (one per client×session)
→ WebSocket binary frame → client
```
