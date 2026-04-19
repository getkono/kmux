set shell := ["bash", "-euo", "pipefail", "-c"]

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
