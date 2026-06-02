set shell := ["bash", "-euo", "pipefail", "-c"]
# Pass recipe arguments as $1, $2, ... so the release recipe can quote them safely.
set positional-arguments

default:
    @just --list

# Maximal debugging: full panic + library backtraces, verbose kmux logs, and
# GLib/GTK diagnostics. Logs stream to stderr (the terminal) so a crash shows
# the live trace next to its backtrace. Each env var below is overridable:
#   just start                         # launch the GUI
#   just start --dry-run myhost        # forward args to the binary
#   RUST_LOG=trace just start          # override the default log filter
#   KMUX_LOG_STDERR=0 just start       # log to the client log file instead
# Run the kmux GUI (debug build), forwarding any args to the `kmux` binary.
start *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export RUST_BACKTRACE="${RUST_BACKTRACE:-full}"
    export RUST_LIB_BACKTRACE="${RUST_LIB_BACKTRACE:-1}"
    export RUST_LOG="${RUST_LOG:-kmux=debug,kmux_app=debug,kmux_client=debug,kmux_protocol=debug,kmux_gtk=debug}"
    export KMUX_LOG_STDERR="${KMUX_LOG_STDERR:-1}"
    export G_MESSAGES_DEBUG="${G_MESSAGES_DEBUG:-all}"
    export RUST_LOG_STYLE="${RUST_LOG_STYLE:-always}"
    cargo run --bin kmux -- "$@"

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

# Rebuild kmux-tui + kmuxd (debug) and restart the daemon via `kmux-tui daemon restart`.
# Binary resolution: the client at target/debug/kmux-tui picks up its sibling
# kmuxd at target/debug/kmuxd, spawning it with the same argv as any auto-spawn.
restart-daemon:
    cargo build -p kmux-tui -p kmuxd
    cargo run -p kmux-tui -- daemon restart

# Start the local daemon (debug build) via the same primitive as auto-spawn.
start-daemon:
    cargo build -p kmux-tui -p kmuxd
    cargo run -p kmux-tui -- daemon start

# Stop the local daemon (debug build).
stop-daemon:
    cargo run -p kmux-tui -- daemon stop

# Install the clients + daemon to ~/.cargo/bin (release build). The GTK GUI
# (`kmux`) is Linux-only; its desktop entry + icon are installed there too.
install:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo install --path crates/kmux-tui
    cargo install --path crates/kmuxd
    if [[ "$(uname -s)" == "Linux" ]]; then
        cargo install --path crates/kmux-gtk
        apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
        icons="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps"
        mkdir -p "$apps" "$icons"
        cp crates/kmux-gtk/data/dev.getkono.kmux.desktop "$apps/"
        cp crates/kmux-gtk/data/icons/dev.getkono.kmux.svg "$icons/"
        echo "==> installed the kmux GUI + desktop entry (GTK4 + libadwaita are runtime deps)"
    fi

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
    cargo build --release -p kmux-tui -p kmuxd
    rm -rf "dist/${stage}"
    mkdir -p "dist/${stage}"
    cp target/release/kmux-tui target/release/kmuxd README.md "dist/${stage}/"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        # Resolve the dylib from kmuxd's baked rpath (authoritative — it is where
        # the binary was linked to look), then ship it and repoint to @loader_path.
        libdir=$(otool -l target/release/kmuxd | awk '$1=="path"{print $2}' | grep -m1 kmux-ghostty-sys || true)
        [[ -n "$libdir" && -f "$libdir/libkmux_ghostty.dylib" ]] || { echo "error: could not locate libkmux_ghostty.dylib (rpath: ${libdir:-none})" >&2; exit 1; }
        cp "$libdir/libkmux_ghostty.dylib" "dist/${stage}/"
        oldref=$(otool -L "dist/${stage}/kmuxd" | awk '/libkmux_ghostty/{print $1; exit}')
        install_name_tool -change "$oldref" @rpath/libkmux_ghostty.dylib "dist/${stage}/kmuxd"
        install_name_tool -add_rpath @loader_path "dist/${stage}/kmuxd"
        strip -x "dist/${stage}/kmux-tui" "dist/${stage}/kmuxd"
    else
        command -v patchelf >/dev/null || { echo "error: patchelf is required to package on Linux (e.g. 'dnf install patchelf' / 'apt-get install patchelf')" >&2; exit 1; }
        libdir=$(patchelf --print-rpath target/release/kmuxd | tr ':' '\n' | grep -m1 kmux-ghostty-sys || true)
        [[ -n "$libdir" && -f "$libdir/libkmux_ghostty.so" ]] || { echo "error: could not locate libkmux_ghostty.so (rpath: ${libdir:-none})" >&2; exit 1; }
        cp "$libdir/libkmux_ghostty.so" "dist/${stage}/"
        patchelf --set-rpath '$ORIGIN' "dist/${stage}/kmuxd"
        strip "dist/${stage}/kmux-tui" "dist/${stage}/kmuxd"
        # The GTK GUI (`kmux`) is Linux-only. It dynamically links the system
        # GTK4 + libadwaita (NOT bundled, unlike libkmux_ghostty) — they are
        # runtime deps the user installs from their distro. Ship the binary plus
        # its desktop entry + icon under share/.
        echo "==> building + staging the kmux GUI"
        cargo build --release -p kmux-gtk
        cp target/release/kmux "dist/${stage}/"
        strip "dist/${stage}/kmux"
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
