# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

- Run server: `cargo run -p kmuxd -- --self-signed`
- Run TUI client: `cargo run -p kmux-tui` (deprecated; headless/SSH + regression oracle)
- Run GTK GUI (primary `kmux` client on Linux): `cargo run -p kmux-gtk` (needs system GTK4 + libadwaita dev libs; if another `pkg-config` shadows the system one, prefix `PKG_CONFIG=/usr/bin/pkg-config`)
- Run native macOS app (`kmux-swift`): `just macos-run` (macOS only; needs Xcode — the GTK GUI is Linux-only). See [docs/building-macos.md](docs/building-macos.md)
- Run tests: `just test`
- Lint: `just clippy`
- Lint fix: `just clippy-fix`
- Format: `just fmt`
- Format check: `just fmt-check`

## Conventions

- The client is layered for multiple frontends: `kmux-protocol` → `kmux-client` (mechanism) → `kmux-app` (toolkit-agnostic interaction policy + `AppCore` + the `FrontendDriver` shared run loop) → frontends: `kmux-tui` (ratatui), `kmux-gtk` (GTK4, **Linux-only**), and `kmux-swift` (native SwiftUI, **macOS-only**) — the last drives `FrontendDriver` across the `kmux-ffi` uniffi C-ABI boundary. `kmux-gtk`'s GTK deps are target-gated to Linux (a stub binary elsewhere); `kmux-swift` is a SwiftPM package outside the cargo workspace. Nothing at or below `kmux-app` may depend on a UI toolkit. See [docs/architecture-frontend.md](docs/architecture-frontend.md).
- Any architectural detail/change should be documented in `docs/` directory.
- Use strict Rust -- no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors

## Correctness (IMPORTANT!)

- Every component that interacts with external dependencies is versioned. For instance, the data protocol is versioned so `kmux` refuses to talk to `kmuxd` instance unless it matches. The `kmux-ffi` C ABI carries `KMUX_FFI_ABI_VERSION` (asserted by the Swift wrapper, alongside uniffi's binding-checksum check), like `kmux-ghostty-sys`'s `EXPECTED_ABI_VERSION`.
