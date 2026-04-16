# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

- Run server: `cargo run -p kmuxd -- --self-signed`
- Run TUI client: `cargo run -p kmux`
- Run tests: `just test`
- Lint: `just clippy`
- Lint fix: `just clippy-fix`
- Format: `just fmt`
- Format check: `just fmt-check`

## Architecture

- See [docs/connection.md](docs/connection.md) for a full description of the
  two-phase connection model, transport selection, supervisor scoring,
  `kmuxd.toml` configuration, and the SSH bootstrap flow.
- See [docs/PERFORMANCE.md](docs/PERFORMANCE.md) for latency measurement and
  scorer behavior notes.

## Conventions

- Use strict Rust -- no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors
