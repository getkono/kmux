set shell := ["bash", "-euo", "pipefail", "-c"]
# Pass recipe arguments as $1, $2, ... so the release recipe can quote them safely.
set positional-arguments

default:
    @just --list

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

# Rebuild kmux + kmuxd (debug) and restart the daemon via `kmux daemon restart`.
# Binary resolution: kmux at target/debug/kmux picks up its sibling kmuxd at
# target/debug/kmuxd, spawning it with the same argv as any auto-spawn.
restart-daemon:
    cargo build -p kmux -p kmuxd
    cargo run -p kmux -- daemon restart

# Start the local daemon (debug build) via the same primitive as auto-spawn.
start-daemon:
    cargo build -p kmux -p kmuxd
    cargo run -p kmux -- daemon start

# Stop the local daemon (debug build).
stop-daemon:
    cargo run -p kmux -- daemon stop

# Install kmux and kmuxd to ~/.cargo/bin (release build)
install:
    cargo install --path crates/kmux
    cargo install --path crates/kmuxd

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
