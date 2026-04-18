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

# Stop any running kmuxd (debug), rebuild, and restart it
restart-daemon:
    #!/usr/bin/env bash
    set -euo pipefail
    PID_FILE="${XDG_RUNTIME_DIR:-/tmp}/kmux-debug/daemon.pid"
    if [[ -f "$PID_FILE" ]]; then
        PID=$(cat "$PID_FILE")
        if kill -0 "$PID" 2>/dev/null; then
            echo "Stopping kmuxd (pid $PID)..."
            kill "$PID"
            for i in $(seq 1 20); do
                kill -0 "$PID" 2>/dev/null || break
                sleep 0.1
            done
            kill -0 "$PID" 2>/dev/null && kill -9 "$PID" || true
        fi
        rm -f "$PID_FILE"
    fi
    cargo build -p kmuxd
    exec cargo run -p kmuxd -- --self-signed
