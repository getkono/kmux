# Building the native macOS app (`kmux-swift`)

`kmux-swift` is the native SwiftUI macOS client — parallel to the GTK4 `kmux-gtk`
client on Linux. It drives the same toolkit-agnostic `FrontendDriver` (in
`kmux-app`) across the **`kmux-ffi`** uniffi C-ABI boundary. See
[architecture-frontend.md](architecture-frontend.md#the-native-macos-frontend-kmux-swift)
for the design; this doc is the build/run recipe.

## Prerequisites

- **Xcode** (or the Command Line Tools) — provides `swift` and the macOS SDK.
- **Rust** — via [mise](https://mise.jdx.dev) (`mise install`), matching CI. The
  `kmux-ffi` crate has **no** Zig/ghostty dependency, so building the app does
  *not* need the Zig toolchain or the `vendor/ghostty` submodule. (Running a
  local daemon — `kmuxd` — for an end-to-end test does; that build uses Zig.)

## Layout

```
kmux-swift/                         # a SwiftPM package, OUTSIDE the cargo workspace
  Package.swift
  Sources/
    kmux_ffiFFI/                    # systemLibrary: the generated C FFI header
      module.modulemap             #   (committed)
      kmux_ffiFFI.h                #   (generated — gitignored)
    KmuxBindings/
      kmux_ffi.swift               # generated Swift bindings (gitignored)
    KmuxApp/                        # the SwiftUI app + CoreText terminal renderer
  Tests/KmuxAppTests/
```

The executable static-links `target/debug/libkmux_ffi.a` plus the system
frameworks the Rust crate graph (rustls / ring / tokio) needs at final link
(`Security`, `SystemConfiguration`, `CoreFoundation`, `libresolv`).

## Generate the bindings

The uniffi Swift bindings (`kmux_ffiFFI.h`, `kmux_ffi.swift`) are **generated, not
committed**. Produce them from the built `kmux-ffi` cdylib (uniffi *library
mode*):

```sh
just gen-ffi-bindings
```

which runs, roughly:

```sh
cargo build -p kmux-ffi
cargo run -p kmux-ffi --bin uniffi-bindgen -- \
  generate --library target/debug/libkmux_ffi.dylib --language swift --out-dir <tmp>
# then copies kmux_ffiFFI.h and kmux_ffi.swift into kmux-swift/Sources/
```

Regenerate after any change to the `kmux-ffi` surface. Drift is caught at runtime
by uniffi's binding-checksum check **and** the app's `KMUX_FFI_ABI_VERSION`
assert (`kmuxFfiAbiVersion() == 1`).

## Build / run / test

```sh
just macos-app     # gen bindings + swift build
just macos-run     # gen bindings + swift run (launches the app)
just macos-test    # gen bindings + swift test
```

(or directly: `swift build --package-path kmux-swift`, etc., after
`just gen-ffi-bindings`).

## End-to-end test against a local daemon

```sh
# In one shell: start a local daemon (this build needs Zig for ghostty).
cargo run -p kmuxd -- --self-signed     # or: kmux-tui daemon start

# In another: launch the app — it connects to the local daemon over the UDS,
# renders the active session, and forwards keystrokes.
just macos-run
```

The app defaults to the local daemon (`DriverConfig.server = nil`). Verify:
typing (incl. arrows / Ctrl-C — confirming daemon-side encoding), resize reflow,
mouse-wheel scrollback + indicator, drag-select + ⌘C / ⌘V, the sessions sidebar
(switch / new / close / rename), pane tabs, the `/`-command palette (⌘P), the
server picker (⌘O), Preferences theme switching (⌘,), the HUD (⌘⇧H), cursor
blink, and the connection badge + reconnect.

## CI

The `macos` job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
fmt-checks, clippies + tests `kmux-ffi` and the client stack it wraps, builds the
`kmux-gtk` Linux-gate stub (proving the gate compiles on macOS), then runs
`just gen-ffi-bindings` + `swift build` + `swift test`. It needs neither the Zig
toolchain nor submodules.

## Notes / limitations

- The app builds/runs as a SwiftPM executable, not yet a codesigned `.app`
  bundle. Launched from a terminal it sets `NSApplication` to a regular
  foreground app.
- The renderer uses the system monospaced face; a configurable font in
  Preferences is a follow-up.
