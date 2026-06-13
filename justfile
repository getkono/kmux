set shell := ["bash", "-euo", "pipefail", "-c"]
# Pass recipe arguments as $1, $2, ... so the release recipe can quote them safely.
set positional-arguments

# Point every recipe at the mise-pinned zig (0.15.2; see mise.toml) so the
# kmux-ghostty-sys build picks it up even when mise is NOT activated in the
# caller's shell (no `mise activate`/shims on PATH) and a different `zig` (e.g.
# a Homebrew one) shadows it. build.rs honors $ZIG; we resolve it via mise and
# fall back to bare `zig` (PATH lookup) when mise can't provide it.
export ZIG := `mise which zig 2>/dev/null || echo zig`

default:
    @just --list

# Maximal debugging: full panic + library backtraces, verbose kmux logs, and
# GLib/GTK diagnostics. Logs stream to stderr (the terminal) so a crash shows
# the live trace next to its backtrace. Each env var below is overridable:
#   just start                         # launch the GUI
#   just start --dry-run myhost        # forward args to the binary
#   RUST_LOG=trace just start          # override the default log filter
#   KMUX_LOG_STDERR=0 just start       # log to the client log file instead
# The `kmux` entrypoint execs its sibling `kmux-gtk` in target/debug, so build both.
# Run the kmux GUI (debug build) via the `kmux` entrypoint, forwarding any args.
start *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"
    export RUST_LIB_BACKTRACE="${RUST_LIB_BACKTRACE:-1}"
    export RUST_LOG="${RUST_LOG:-kmux=debug,kmux_app=debug,kmux_client=debug,kmux_protocol=debug,kmux_gtk=debug}"
    export KMUX_LOG_STDERR="${KMUX_LOG_STDERR:-1}"
    export G_MESSAGES_DEBUG="${G_MESSAGES_DEBUG:-all}"
    export RUST_LOG_STYLE="${RUST_LOG_STYLE:-always}"
    cargo build -p kmux -p kmux-gtk
    cargo run -p kmux -- "$@"

# Format all code
fmt:
    cargo fmt --all

# Check formatting (CI mode)
fmt-check:
    cargo fmt --all -- --check

# Run clippy lints
clippy:
    cargo clippy --all-targets -- -D warnings

# Auto-fix clippy lints
clippy-fix:
    cargo clippy --fix --allow-dirty --allow-staged --all-targets -- -D warnings

# Build (default features)
build:
    cargo build

# Run tests (default features)
test:
    cargo test

# ── Native macOS app (kmux-swift) ────────────────────────────────────────────
# The SwiftUI macOS client lives in kmux-swift/ (a SwiftPM package, outside the
# cargo workspace) and links the kmux-ffi staticlib. These recipes are macOS-only
# (gated like `install`/`package`); on Linux the GTK GUI (`kmux-gtk`) is the client.

# Generate the uniffi Swift bindings from the built kmux-ffi cdylib (library mode).
gen-ffi-bindings profile="debug":
    #!/usr/bin/env bash
    set -euo pipefail
    # The profile (debug|release) selects which built cdylib to introspect;
    # `just install` passes "release" so the bindings match the release staticlib
    # it links. The generated Swift is identical across profiles (it is derived
    # from the crate metadata), so the default debug profile is fine for dev builds.
    [[ "$(uname -s)" == "Darwin" ]] || { echo "error: gen-ffi-bindings is macOS-only (the Swift app is macOS-only)" >&2; exit 1; }
    case "{{profile}}" in
        debug)   relflag="" ;;
        release) relflag="--release" ;;
        *) echo "error: profile must be 'debug' or 'release' (got '{{profile}}')" >&2; exit 1 ;;
    esac
    cargo build $relflag -p kmux-ffi
    out=$(mktemp -d)
    # `--no-format` skips uniffi's optional swiftformat pass: the bindings are
    # machine-generated (and gitignored), so formatting them is pointless, and
    # without it uniffi prints a benign "Unable to auto-format" warning whenever
    # swiftformat isn't installed (see issue #104).
    cargo run -p kmux-ffi --bin uniffi-bindgen -- \
        generate --no-format --library target/{{profile}}/libkmux_ffi.dylib --language swift --out-dir "$out"
    mkdir -p kmux-swift/Sources/kmux_ffiFFI kmux-swift/Sources/KmuxBindings
    cp "$out/kmux_ffiFFI.h"  kmux-swift/Sources/kmux_ffiFFI/kmux_ffiFFI.h
    cp "$out/kmux_ffi.swift" kmux-swift/Sources/KmuxBindings/kmux_ffi.swift
    echo "==> generated Swift bindings into kmux-swift/Sources/"

# Build the native macOS app (kmux-swift). Regenerates bindings first.
swift-app: gen-ffi-bindings
    swift build --package-path kmux-swift

# Run the native macOS app (kmux-swift).
swift-run: gen-ffi-bindings
    swift run --package-path kmux-swift

# Test the native macOS app (kmux-swift).
swift-test: gen-ffi-bindings
    swift test --package-path kmux-swift

# ── GTK app (kmux-gtk) ───────────────────────────────────────────────────────
# The GTK4 + libadwaita client lives in crates/kmux-gtk and is the default client
# on Linux, but also runs on macOS (needs Homebrew GTK4 + libadwaita:
# `brew install gtk4 libadwaita`). If another pkg-config shadows the system one,
# prefix these recipes with `PKG_CONFIG=/usr/bin/pkg-config`.

# Build the GTK app (kmux-gtk).
gtk-app:
    cargo build -p kmux-gtk

# Run the GTK app (kmux-gtk).
gtk-run:
    cargo run -p kmux-gtk

# Test the GTK app (kmux-gtk).
gtk-test:
    cargo test -p kmux-gtk

# Generate docs
doc:
    cargo doc --no-deps --open

# Check docs build (no open)
doc-check:
    cargo doc --no-deps --all-features

# Run doc tests
doc-test:
    cargo test --doc --all-features

# Tail the client log (kmux)
tail-client-log:
    tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/kmux/client.log"

# Tail the daemon log (kmuxd)
tail-daemon-log:
    tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/kmux/daemon.log"

# Binary resolution: the toolkit-free `kmux` client at target/debug/kmux picks up
# its sibling kmuxd at target/debug/kmuxd, spawning it with the same argv as any
# auto-spawn. `kmux daemon …` never loads a UI toolkit. Forwarded args go straight
# to the subcommand (which uses the same primitive as auto-spawn):
#   just daemon start            # start the local daemon
#   just daemon stop             # stop it
#   just daemon restart          # rebuild + restart
#   just daemon status           # query status
# Rebuild kmux + kmuxd (debug) and run `kmux daemon <args>`.
daemon *args:
    cargo build -p kmux -p kmuxd
    cargo run -p kmux -- daemon "$@"

# Upgrade a running daemon in place: build + install a fresh release kmuxd, then
# live-restart it so existing sessions (shells, editors, REPLs) survive (issue #36).
# Unlike `just daemon restart` (which only rebuilds + restarts a dev daemon), this swaps
# the installed system binary, so it is the path to ship a new daemon to a live
# session. See docs/daemon-handoff.md §"Upgrading a running daemon".
upgrade-daemon:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v kmux >/dev/null || { echo "error: 'kmux' not found on PATH; run 'just install' first (this recipe upgrades an *installed* daemon)" >&2; exit 1; }
    # Rebuild the release kmuxd. This also refreshes the build-tree libkmux_ghostty
    # that the installed binary's rpath points at — `cargo install` does NOT
    # bundle/repoint the dylib the way `just package` does, so a fresh build keeps
    # the new daemon's library ABI-matched (kmux-ghostty-sys EXPECTED_ABI_VERSION).
    cargo build --release -p kmuxd
    # Atomic in-place replace of ~/.cargo/bin/kmuxd. The live restart below re-execs
    # the *running* daemon's own binary path (see handoff::sender::resolve_successor_exe),
    # so an in-place upgrade only takes effect when the running daemon IS this
    # installed binary — a dev daemon launched from target/debug/kmuxd would re-exec
    # the debug build, not this one.
    cargo install --path crates/kmuxd
    # Trigger the graceful handoff: the new binary is spawned as a successor, every
    # live PTY master fd is streamed to it via SCM_RIGHTS, then the old daemon exits;
    # connected clients reconnect with the adopted token. `kmux daemon restart`
    # starts the daemon if none is running. It is the success gate (non-zero on a
    # failed/timed-out handoff); the status prints are informational only.
    echo "==> before:"; kmux daemon status || true
    kmux daemon restart
    echo "==> after:";  kmux daemon status || true

# Install the clients + daemon for the host platform (release build).
install:
    #!/usr/bin/env bash
    set -euo pipefail
    # The `kmux` entrypoint + the `kmuxd` daemon go to ~/.cargo/bin on every
    # platform; the desktop GUI is installed the platform-native way and `kmux`
    # (no args) opens it.
    #   - Linux: the GTK frontend `kmux-gtk` to ~/.cargo/bin (the entrypoint execs
    #            it), plus its .desktop entry + icon into the XDG data dirs
    #            (Activities / app grid).
    #   - macOS: the SwiftUI app assembled into ~/Applications/kmux.app (Launchpad
    #            / Spotlight / Dock), bundling kmuxd beside it so a Finder-launched
    #            app can auto-spawn the local daemon; the `kmux` entrypoint execs
    #            the bundle so `kmux` from a terminal starts the GUI too.
    cargo install --path crates/kmux
    cargo install --path crates/kmuxd
    if [[ "$(uname -s)" == "Linux" ]]; then
        cargo install --path crates/kmux-gtk
        apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
        icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
        mkdir -p "$apps" "$icons"
        cp crates/kmux-gtk/data/dev.getkono.kmux.desktop "$apps/"
        cp crates/kmux-gtk/data/icons/dev.getkono.kmux.svg "$icons/"
        echo "==> installed kmux + the kmux-gtk GUI + desktop entry (GTK4 + libadwaita are runtime deps)"
    elif [[ "$(uname -s)" == "Darwin" ]]; then
        # Build the release FFI staticlib + matching Swift bindings, then the app
        # in release linking that archive (KMUX_FFI_LIB overrides Package.swift's
        # debug default to an absolute release path).
        just gen-ffi-bindings release
        KMUX_FFI_LIB="$PWD/target/release/libkmux_ffi.a" \
            swift build -c release --package-path kmux-swift
        exe="kmux-swift/.build/release/kmux-swift"
        [[ -x "$exe" ]] || { echo "error: swift build did not produce $exe" >&2; exit 1; }
        ver=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' Cargo.toml | head -n1)
        # Assemble the kmux.app bundle in ~/Applications (user-level, no sudo) —
        # the macOS analog of the Linux .desktop entry + icon.
        app="$HOME/Applications/kmux.app"
        rm -rf "$app"
        mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
        cp "$exe" "$app/Contents/MacOS/kmux-swift"
        # Bundle kmuxd beside the GUI exe: find_server_binary() looks in the
        # running exe's own dir first, so a Finder/Spotlight launch (which gets
        # the minimal launchd PATH, without ~/.cargo/bin) can still auto-spawn
        # the local daemon. Its rpath points back into the build tree for
        # libkmux_ghostty, same as the ~/.cargo/bin/kmuxd from `cargo install`.
        cp target/release/kmuxd "$app/Contents/MacOS/kmuxd"
        cp kmux-swift/macos/kmux.icns "$app/Contents/Resources/kmux.icns"
        sed "s/__VERSION__/${ver}/g" kmux-swift/macos/Info.plist > "$app/Contents/Info.plist"
        # Refresh Launch Services so Finder/Spotlight pick up the new bundle + icon.
        touch "$app"
        /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$app" 2>/dev/null || true
        # No separate launcher script: the `kmux` entrypoint installed above
        # (cargo install --path crates/kmux) execs this bundle, so `kmux` from a
        # terminal starts the GUI.
        echo "==> installed kmux.app to ~/Applications (launch from Launchpad/Spotlight); the 'kmux' entrypoint execs it"
    fi
    # Dynamic shell completion is built into the `kmux` binary (clap_complete's
    # CompleteEnv) — there are no completion files to install, just one line to
    # add per shell. We only print instructions (never edit rc files) so the
    # change is transparent and reversible.
    echo
    echo "==> Optional: enable dynamic tab-completion for kmux. Add the line for"
    echo "    your shell, then restart it (or re-source the rc file):"
    echo
    echo '      bash   (~/.bashrc):                  source <(COMPLETE=bash kmux)'
    echo '      zsh    (~/.zshrc):                   source <(COMPLETE=zsh kmux)'
    echo '      fish   (~/.config/fish/config.fish): COMPLETE=fish kmux | source'
    echo
    echo "    Completes subcommands, flags, themes, hosts.toml aliases, and live"
    echo "    daemon sessions. See docs/shell-completion.md (Elvish/PowerShell too)."

# Stage a distributable release tarball for the host target into dist/.
package:
    #!/usr/bin/env bash
    set -euo pipefail
    # kmuxd dynamically links libkmux_ghostty (a Zig-built shared lib; see
    # crates/kmux-ghostty-sys), so we bundle that .so/.dylib beside the binaries
    # and rewrite kmuxd's runpath to load it from its own dir ($ORIGIN /
    # @loader_path) -- otherwise the binary only runs from the build tree. Shared
    # by local testing and the release workflow (.github/workflows/release.yml).
    ver=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' Cargo.toml | head -n1)
    target=$(rustc -vV | sed -n 's/^host: //p')
    stage="kmux-${ver}-${target}"
    echo "==> building release binaries (${target})"
    cargo build --release -p kmux -p kmuxd
    rm -rf "dist/${stage}"
    mkdir -p "dist/${stage}"
    cp target/release/kmux target/release/kmuxd README.md "dist/${stage}/"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        # Resolve the dylib from kmuxd's baked rpath (authoritative — it is where
        # the binary was linked to look), then ship it and repoint to @loader_path.
        libdir=$(otool -l target/release/kmuxd | awk '$1=="path"{print $2}' | grep -m1 kmux-ghostty-sys || true)
        [[ -n "$libdir" && -f "$libdir/libkmux_ghostty.dylib" ]] || { echo "error: could not locate libkmux_ghostty.dylib (rpath: ${libdir:-none})" >&2; exit 1; }
        cp "$libdir/libkmux_ghostty.dylib" "dist/${stage}/"
        oldref=$(otool -L "dist/${stage}/kmuxd" | awk '/libkmux_ghostty/{print $1; exit}')
        install_name_tool -change "$oldref" @rpath/libkmux_ghostty.dylib "dist/${stage}/kmuxd"
        install_name_tool -add_rpath @loader_path "dist/${stage}/kmuxd"
        strip -x "dist/${stage}/kmux" "dist/${stage}/kmuxd"
    else
        command -v patchelf >/dev/null || { echo "error: patchelf is required to package on Linux (e.g. 'dnf install patchelf' / 'apt-get install patchelf')" >&2; exit 1; }
        libdir=$(patchelf --print-rpath target/release/kmuxd | tr ':' '\n' | grep -m1 kmux-ghostty-sys || true)
        [[ -n "$libdir" && -f "$libdir/libkmux_ghostty.so" ]] || { echo "error: could not locate libkmux_ghostty.so (rpath: ${libdir:-none})" >&2; exit 1; }
        cp "$libdir/libkmux_ghostty.so" "dist/${stage}/"
        patchelf --set-rpath '$ORIGIN' "dist/${stage}/kmuxd"
        strip "dist/${stage}/kmux" "dist/${stage}/kmuxd"
        # The GTK frontend (`kmux-gtk`) is the default client on Linux; the `kmux`
        # entrypoint (shipped above) execs it. It dynamically links the system
        # GTK4 + libadwaita (NOT bundled, unlike libkmux_ghostty) — they are
        # runtime deps the user installs from their distro. Ship the binary plus
        # its desktop entry + icon under share/.
        echo "==> building + staging the kmux-gtk GUI"
        cargo build --release -p kmux-gtk
        cp target/release/kmux-gtk "dist/${stage}/"
        strip "dist/${stage}/kmux-gtk"
        mkdir -p "dist/${stage}/share/applications" \
                 "dist/${stage}/share/icons/hicolor/scalable/apps"
        cp crates/kmux-gtk/data/dev.getkono.kmux.desktop "dist/${stage}/share/applications/"
        cp crates/kmux-gtk/data/icons/dev.getkono.kmux.svg "dist/${stage}/share/icons/hicolor/scalable/apps/"
    fi
    tar -C dist -czf "dist/${stage}.tar.gz" "${stage}"
    ( cd dist && shasum -a 256 "${stage}.tar.gz" > "${stage}.tar.gz.sha256" )
    echo "==> packaged dist/${stage}.tar.gz"
    ls -l "dist/${stage}.tar.gz" "dist/${stage}.tar.gz.sha256"

# Cut a release: bump version, regenerate CHANGELOG, commit, tag v<ver>, push.
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    # Pushing the tag triggers the release workflow, which builds binaries for all
    # targets and publishes the GitHub Release. Idempotent / re-runnable: the same
    # version converges (no-op bump/commit) and force-repushes the tag to
    # re-trigger the workflow (so a failed or yanked release can be re-cut). Run
    # from a clean master checkout:  just release 0.2.0
    ver="$1"
    if [[ ! "$ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$ ]]; then
        echo "error: '$ver' is not valid semver (expected MAJOR.MINOR.PATCH, no leading 'v')" >&2
        exit 1
    fi
    tag="v${ver}"
    branch=$(git rev-parse --abbrev-ref HEAD)
    [[ "$branch" == "master" ]] || { echo "error: releases must be cut from master (on '$branch')" >&2; exit 1; }
    if ! git diff --quiet || ! git diff --cached --quiet; then
        echo "error: working tree not clean; commit or stash changes first" >&2
        exit 1
    fi
    echo "==> fetching tags"
    git fetch --quiet origin --tags
    echo "==> setting workspace version to ${ver}"
    # Anchored to the [workspace.package] block so [workspace.dependencies] versions are untouched.
    sed -i.bak -E '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "'"$ver"'"/' Cargo.toml
    rm -f Cargo.toml.bak
    # Sync the workspace member entries in Cargo.lock without bumping external deps.
    cargo update --workspace --offline
    echo "==> regenerating CHANGELOG.md"
    git cliff --tag "$tag" -o CHANGELOG.md
    git add -A
    if git diff --cached --quiet; then
        echo "==> nothing to commit (already at ${tag} state)"
    else
        git commit -q -m "chore(release): ${tag}"
        echo "==> committed chore(release): ${tag}"
    fi
    echo "==> tagging ${tag} at HEAD"
    git tag -fa "$tag" -m "$tag"
    echo "==> pushing ${branch} (runs the pre-push fmt/clippy/test gate)"
    git push origin "$branch"
    echo "==> pushing ${tag} (force; re-triggers the release workflow)"
    git push --force --no-verify origin "$tag"
    echo "==> done"
    echo "    workflow: https://github.com/getkono/kmux/actions/workflows/release.yml"
    echo "    release:  https://github.com/getkono/kmux/releases/tag/${tag}"
