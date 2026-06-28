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

Client and daemon compute every path above through the **same**
`kmux_protocol::dirs` helpers (`runtime_dir` / `socket_path` /
`data_socket_path`), namespaced by the compile-time `KMUX_DIR_NAME`
(`kmux` vs `kmux-debug`), so the two can never disagree on which socket to use —
a debug client physically cannot reach a release daemon. The
`runtime_paths_are_profile_isolated` test in `dirs.rs` pins this.

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

`find_server_binary()` (in `kmux-connect::daemon::lifecycle`) resolves the
`kmuxd` to spawn in precedence order: the `KMUX_KMUXD` override → the sibling
`kmuxd` (same directory as the running executable) → **debug builds only:** the
build-time `target/<profile>/kmuxd` → a `$PATH` walk.  Cargo places all binaries
for a profile in the same `target/{debug,release}/` tree, so the sibling rule
picks the same-profile daemon for an installed layout.

The debug-only `target/<profile>` step exists because the sibling rule does *not*
hold for every dev launch: the macOS Swift app's `current_exe()` lives in
`kmux-swift/.build/` (no `kmuxd` sibling), and a bare `cargo run` may not have
built `kmuxd` yet.  Without that step the resolver fell through to `$PATH` and
auto-spawned an installed **release** `~/.cargo/bin/kmuxd`; that daemon binds its
socket under `kmux/` while the debug client polls `kmux-debug/`, so the two never
meet and the GUI reports "daemon start failed".  The dev entrypoint `./kmux`
(via `mise run dev`) also builds `kmuxd` and exports
`KMUX_KMUXD=target/debug/kmuxd` to pin it explicitly.

### Finding the active profile's logs

Because the logs are profile-specific, a `cargo run` GUI writes
`kmux-debug/client.log` while an installed build writes `kmux/client.log` — a
common source of "the error isn't in the log" confusion.  Two helpers cut
through it:

- `kmux debug paths` — run the binary in question; it prints its *own*
  profile's client/daemon logs, runtime/state dirs, and the `kmuxd` an
  auto-spawn would launch.
- `mise run tail-client-log` / `mise run tail-daemon-log` — follow **both**
  the `kmux/` and `kmux-debug/` logs at once.

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
