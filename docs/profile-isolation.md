# Debug / Release Profile Isolation

## Problem

A developer machine typically has both an _installed_ (release) kmux and a
`cargo run` (debug) kmux in flight simultaneously. Without isolation the two
profiles share state files, and interference follows:

- Debug log output mixes with production `daemon.log` and `client.log`.
- Debug daemon writes and reads the same `sessions/state.bin` that the release
  daemon uses; if the schema is mid-flight in a development branch the files
  are incompatible.
- `metrics.jsonl` rows from throwaway debug sessions pollute release telemetry.
- `recent_servers.json` UI state cross-contaminates between profiles.

## Design

The core mechanism is a single constant in `kmux-protocol::dirs`:

```rust
#[cfg(debug_assertions)]
pub const KMUX_DIR_NAME: &str = "kmux-debug";
#[cfg(not(debug_assertions))]
pub const KMUX_DIR_NAME: &str = "kmux";
```

`KMUX_DIR_NAME` is the leaf directory appended to every XDG base that holds
_mutable runtime/state_ data. It is the **only** place in the codebase that
gates on `debug_assertions`; every higher-level path helper derives its
isolation from it automatically.

### What is isolated (profile-specific)

| Base | Path | Contents |
|------|------|----------|
| `$XDG_RUNTIME_DIR` | `kmux[-debug]/daemon.sock` | Control UDS socket |
| | `kmux[-debug]/daemon-data.sock` | Data UDS socket |
| | `kmux[-debug]/daemon.pid` | PID lockfile |
| | `kmux[-debug]/token` | Auth token |
| | `kmux[-debug]/kmuxd-boot.log` | Daemon boot log (captured stdout/stderr) |
| `$XDG_STATE_HOME` | `kmux[-debug]/daemon.log` | Daemon tracing log |
| | `kmux[-debug]/client.log` | Client tracing log |
| | `kmux[-debug]/connections/<id>.log` | Per-connection metadata log |
| | `kmux[-debug]/metrics.jsonl` | Rolling metrics JSONL (+ rotated `.1`) |
| | `kmux[-debug]/sessions/state.bin` | Daemon session checkpoint |
| | `kmux[-debug]/recent_servers.json` | Recent servers UI cache |

### What is shared (intentionally)

Config files represent _user intent_ and should be consistent regardless of
which profile is running:

| Base | Path | Contents |
|------|------|----------|
| `$XDG_CONFIG_HOME` | `kmux/config.toml` | Client theme / settings |
| | `kmux/themes/` | User-installed themes |
| | `kmux/hosts.toml` | Named host list |
| | `kmux/known_hosts.toml` | TOFU TLS certificate pins |
| | `kmux/tls/` | User-supplied TLS cert cache |
| | `kmuxd/kmuxd.toml` | Daemon configuration |

## Profile matching for local connections

The UDS path is itself the match signal.  When a debug `kmux` calls
`ensure_daemon()` it queries `kmux_protocol::dirs::socket_path()`, which
resolves to the `kmux-debug` runtime subdirectory.  If no debug daemon is
listening there, `start_daemon()` spawns one.

`find_server_binary()` (in `kmux-client::daemon::lifecycle`) prefers the
sibling `kmuxd` binary — i.e. the binary in the same directory as the running
`kmux` executable.  Cargo places all binaries for a profile in the same
`target/{debug,release}/` tree, so the sibling rule reliably picks the
same-profile daemon.

For remote (SSH / TCP / QUIC) connections the profile of the remote daemon is
irrelevant; `PROTOCOL_VERSION` (u32) remains the only wire-level compatibility
gate.

## Port defaults

All TCP and QUIC listeners bind port `0` (ephemeral) by default.  The kernel
assigns a free port and the daemon announces the actual bound port to clients.

Port values are **not** configurable in `kmuxd.toml`; the only way to pin a
specific port is the `--port` (QUIC) and `--tcp-port` (TCP+TLS) CLI flags.
This prevents two manually-run daemons (debug and release) from colliding on a
fixed port.
