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

# Tail the client log (kmux / kmux-gui)
tail-client-log:
    tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/kmux/client.log"

# Tail the daemon log (kmuxd)
tail-daemon-log:
    tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/kmux/daemon.log"
