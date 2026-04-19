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

## Conventions

- Any architectural detail/change should be documented in `docs/` directory.
- Use strict Rust -- no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors

## Correctness (IMPORTANT!)

- Every component that interacts with external dependencies is versioned. For instance, the data protocol is versioned so `kmux` refuses to talk to `kmuxd` instance unless it matches.
