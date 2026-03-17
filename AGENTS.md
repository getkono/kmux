# smux

A terminal multiplexer / session manager.

## Tech Stack

- **Runtime:** Rust
- **Language:** Rust
- **Formatter:** rustfmt
- **Linter:** Clippy

## Project Structure

```
smux/
 .github/
    workflows/
        ci.yml
 src/
    main.rs
 .gitignore
 AGENTS.md
 Cargo.toml
 CLAUDE.md -> AGENTS.md
 lefthook.yml
```

## Development

### Setup

```bash
# Rust toolchain (via rustup)
rustup toolchain install stable
```

### Run

```bash
cargo run
```

### Test

```bash
cargo test
```

### Format

```bash
cargo fmt
```

### Lint

```bash
cargo clippy
```

### Lint Fix

```bash
cargo clippy --fix
```

## Conventions

- Use strict Rust -- no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors

## Architecture

smux is a terminal multiplexer / session manager. The intended architecture follows a
server/client split: a background daemon manages sessions and windows, while a thin
client communicates with it over a Unix socket. Sessions contain one or more windows;
windows contain panes that each run a PTY process.
