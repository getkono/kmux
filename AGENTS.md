# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

- Run server: `cargo run -p kmuxd -- --self-signed`
- Run `kmux` (the entrypoint — CLI + opens the platform desktop app): `cargo run -p kmux`. Toolkit-free; handles `daemon`/`ls`/`--dry-run` itself and, for an interactive launch, execs `kmux-gtk` (Linux) or the Swift `kmux.app` (macOS). For a dev GUI run also build the frontend so the exec target exists (`just start`, which builds `kmux` + `kmux-gtk`).
- Run GTK GUI directly (`kmux-gtk` — the default + official client on Linux, also runnable on macOS): `cargo run -p kmux-gtk` (needs GTK4 + libadwaita dev libs: system packages on Linux, `brew install gtk4 libadwaita` on macOS; if another `pkg-config` shadows the system one, prefix `PKG_CONFIG=/usr/bin/pkg-config`)
- Run native macOS app (`kmux-swift` — the default GUI `kmux` opens on macOS): `just swift-run` (macOS only; needs Xcode). See [docs/building-macos.md](docs/building-macos.md)
- Run tests: `just test`
- Lint: `just clippy`
- Lint fix: `just clippy-fix`
- Format: `just fmt`
- Format check: `just fmt-check`

## Conventions

- The client is layered for multiple frontends: `kmux-protocol` → `kmux-client` (mechanism) → `kmux-app` (toolkit-agnostic interaction policy + `AppCore` + the `FrontendDriver` shared run loop + the shared CLI front door `run_cli`) → frontends: `kmux-gtk` (GTK4, **Linux + macOS**) and `kmux-swift` (native SwiftUI, **macOS-only**) — the latter drives `FrontendDriver` across the `kmux-ffi` uniffi C-ABI boundary. Above the frontends sits **`kmux`**, the toolkit-agnostic entrypoint binary (CLI + launcher): it runs the shared subcommands and, for an interactive launch, execs the platform desktop app (`kmux-gtk` on Linux, the Swift `kmux.app` on macOS). `kmux-gtk`'s GTK deps are target-gated to Linux + macOS (a stub binary on other OSes; macOS needs Homebrew GTK4 + libadwaita); `kmux-swift` is a SwiftPM package outside the cargo workspace. Nothing at or below `kmux-app` may depend on a UI toolkit (`kmux` sits above it and stays toolkit-free regardless). See [docs/architecture-frontend.md](docs/architecture-frontend.md).
- Any architectural detail/change should be documented in `docs/` directory.
- Use strict Rust -- no `#[allow(unused)]` without justification
- Write tests for all new functionality
- Use conventional commits (`type: description`)
- Keep functions small and focused
- Prefer `thiserror` for error types, `anyhow` for application-level errors

## Correctness (IMPORTANT!)

- Every component that interacts with external dependencies is versioned. For instance, the data protocol is versioned so `kmux` refuses to talk to `kmuxd` instance unless it matches. The `kmux-ffi` C ABI carries `KMUX_FFI_ABI_VERSION` (asserted by the Swift wrapper, alongside uniffi's binding-checksum check), like `kmux-ghostty-sys`'s `EXPECTED_ABI_VERSION`.
