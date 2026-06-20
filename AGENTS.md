# kmux

A terminal multiplexer / session manager with remote desktop capabilities.

## Commands

- Run server: `cargo run -p kmuxd` (generates a self-signed cert by default)
- Tasks run via mise (`mise run <task>`, replacing the old `just <task>`); `mise tasks` lists them. Git hooks are managed by `hk` (config in `hk.pkl`), installed by `mise install` (or `mise run setup`).
- Run `kmux` (the entrypoint — CLI + opens the platform desktop app): `cargo run -p kmux`. Toolkit-free; handles `daemon`/`ls`/`--dry-run` itself and, for an interactive launch, execs `kmux-gtk` (Linux) or the Swift `kmux.app` (macOS). For a dev GUI run also build the frontend so the exec target exists (`mise run start`, which builds `kmux` + `kmux-gtk`).
- Run GTK GUI directly (`kmux-gtk` — the default + official client on Linux, also runnable on macOS): `cargo run -p kmux-gtk` (needs GTK4 + libadwaita dev libs: system packages on Linux, `brew install gtk4 libadwaita` on macOS; if another `pkg-config` shadows the system one, prefix `PKG_CONFIG=/usr/bin/pkg-config`)
- Run native macOS app (`kmux-swift` — the default GUI `kmux` opens on macOS): `mise run swift-run` (macOS only; needs Xcode). See [docs/building-macos.md](docs/building-macos.md)
- Render diagnostics (issue #145): `kmux diagnostic <test>` opens the GUI with a session painting a known test pattern (`glyphs`/`attrs`/`colors`/`unicode`/`boxes`/`all`) so glyph/color rendering can be visually verified; `kmux diagnostic` lists them and `--emit` writes the pattern to the host terminal. `progress` (issue #125) is an extra, *animated* test that emits OSC 9;4 progress states to verify the per-pane progress bar — it loops over time and so is excluded from `all`. Local-daemon scoped. See [docs/architecture-render.md](docs/architecture-render.md).
- GPU terminal rendering (issue #132): the shared `kmux-render` crate (wgpu) replaces the per-frontend CPU rasterizers. The `gpu` feature is **on by default** now (compiled + tested everywhere; build `--no-default-features` — or `mise run build-no-gpu` — for the lean, wgpu-free path that CI also checks). Only the runtime switch is opt-in: select the GPU path with `KMUX_RENDERER=wgpu cargo run -p kmux-gtk`; Swift uses `mise run swift-gpu-run`. Runtime defaults stay Cairo/CoreText. See [docs/architecture-render.md](docs/architecture-render.md).
- Dev daemon + logs: the GUI run tasks (`mise run gtk-run` / `swift-run` / `start`) build `kmuxd` and pin `KMUX_KMUXD=target/debug/kmuxd` so a **debug** GUI auto-spawns the **debug** daemon (not an installed release `kmuxd` on `$PATH`, which it can't reach — debug builds isolate runtime/state under `kmux-debug/`). Debug builds therefore log to `~/.local/state/kmux-debug/client.log`, not `kmux/client.log`; `kmux debug paths` prints the active profile's resolved log/state/runtime paths + the `kmuxd` it would spawn, and `mise run tail-client-log` / `tail-daemon-log` follow both profiles' logs. See [docs/profile-isolation.md](docs/profile-isolation.md).
- Run tests: `mise run test`
- Lint: `mise run clippy`
- Lint fix: `mise run clippy-fix`
- Format: `mise run fmt`
- Format check: `mise run fmt-check`

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
