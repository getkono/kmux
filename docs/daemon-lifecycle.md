# kmuxd Daemon Lifecycle

This document describes the complete lifecycle of the `kmuxd` daemon: from the
first instruction executed to the last byte written on shutdown.  File paths
are relative to `crates/kmuxd/src/` unless noted.

---

## 1. Entry Point

`main.rs::main()` is the synchronous entry point.  It runs *before* a Tokio
runtime is created so that the optional double-fork daemonization step is
fork-safe.

### 1.1 CLI Parsing

```
kmuxd [flags] [subcommand]

Subcommands:
  probe-or-start   Query/start daemon, print JSON endpoint info to stdout
  print-config     Dump resolved config and exit

Server flags (all deprecated in favour of kmuxd.toml):
  --bind / --port / --cert / --key / --daemon / --tcp-port
```

Deprecated per-flag overrides are applied *after* loading the config file and
win when present (backward compatibility for existing scripts).

### 1.2 Short-Lived Subcommands

`probe-or-start` and `print-config` create their own minimal Tokio runtimes,
complete, and exit before the main server path is entered.

---

## 2. Daemonization

If `--daemon` is present, `daemon::daemonize_process(&pid_path)` is called
**before** `tokio::runtime::Runtime::new()`.  This is mandatory because `fork`
and multi-threaded async runtimes are incompatible.

```
daemonize_process(pid_path):
  Uses the `daemonize` crate (double-fork)
  Working directory → /
  Umask → 0o077
  Writes child PID to $XDG_RUNTIME_DIR/kmux/kmuxd.pid
  Returns in the daemonized child; parent exits
```

After this call the process is a daemon with fresh file descriptors.

---

## 3. Logging Initialization

Tracing is initialized after daemonization (so the child's fresh fds are used).

```
Attempt to open $XDG_RUNTIME_DIR/kmux/kmuxd.log (append mode)
  Success → log to file
  Failure → log to stderr
Log level: RUST_LOG env var, default "kmuxd=info"
Each run tagged with a random 4-byte instance_id for log correlation
```

---

## 4. Configuration Loading

`config::load_config(cli.config)` searches for `kmuxd.toml` in order:

1. `--config <path>` (explicit override)
2. `$XDG_CONFIG_HOME/kmuxd/kmuxd.toml`
3. `/etc/kmuxd/kmuxd.toml`
4. Built-in defaults (if none found)

On first run with no config file found, a default template is written to
`$XDG_CONFIG_HOME/kmuxd/kmuxd.toml`.

`ServerConfig::resolve()` validates the result: at least one enabled listener;
a TLS `cert` requires a matching `key` (and vice versa). With neither set, the
daemon generates an in-memory self-signed certificate (issue #100).

### Key Config Sections

```toml
[tls]
# Omit cert/key for an auto-generated in-memory self-signed cert (the default).
cert = "/path/cert.pem"     # custom certificate (optional)
key  = "/path/key.pem"

[[listen]]
kind     = "quic"           # | "tcp+tls" | "unix"
bind     = "::"
port     = 8443             # 0 = OS-assigned ephemeral port
enabled  = true
audience = "any"            # any | local | lan | ssh-only

[daemon]
idle_shutdown_secs = 30     # 0 = disabled

[advertise]
public_host = "example.com" # override advertised hostname
```

---

## 5. Async Initialization (`startup::async_main`)

The Tokio runtime is created and `startup::async_main(daemon, cfg)` is called.
Steps execute sequentially unless noted.

### 5.1 TLS Material

```
if cfg.tls.cert and cfg.tls.key:
    CertMaterial::from_files(cert_path, key_path)
else:
    CertMaterial::self_signed()   # default: rcgen EC key + cert, in-memory only
```

The same `CertMaterial` is cloned for each QUIC and TCP+TLS listener.

### 5.2 Auth Token Generation

```
token = generate_token()          # 32 random bytes → 64-char hex string
persist_token(&token)             # writes to $XDG_RUNTIME_DIR/kmux/token (0600)
println!("Auth token: {token}")   # printed to stdout for SSH callers
Arc<ServerApp>::new(token)        # embeds token in server state
```

The token is single-use per daemon run; it changes on every restart.

### 5.3 Session Restore (Optional)

If `$XDG_RUNTIME_DIR/kmux/session_state.bin` exists, it is read and the
previous sessions are restored before accepting new connections.  See
[Section 10](#10-persistence--checkpointing) and
[Section 11](#11-session-restore-on-startup).

### 5.4 Periodic Checkpoint Task

A background Tokio task checkpoints session state every 30 seconds:

```
tokio::spawn:
  loop:
    interval.tick() // every 30 s, MissedTickBehavior::Skip
    state = app.checkpoint_state().await
    write_checkpoint(&state, session_state_path)
```

Missed ticks are skipped so a slow checkpoint does not cause thundering writes.

### 5.5 Listener Binding

Each enabled `[[listen]]` entry is bound in order:

| Kind      | Bind call                                   | TLS |
|-----------|---------------------------------------------|-----|
| `quic`    | `quinn::Endpoint::server(quinn_cfg, addr)`  | Yes |
| `tcp+tls` | `TlsTcpListener::bind(addr, tls_cfg).await` | Yes |
| `unix`    | `UdsListener::bind(path)`                   | No  |

Port 0 is resolved to the OS-assigned port; `resolved_listeners[i].port` is
updated so `announce.rs` can advertise the real port.

`UDS` path `"auto"` resolves to
`$XDG_RUNTIME_DIR/kmux/kmux.sock`.

### 5.6 Accept Loop Tasks

One background Tokio task is spawned per bound listener:

```
tokio::spawn:
  loop:
    session = listener.accept().await
      Ok(session)         → tokio::spawn(dispatch_session(session, app))
      Err(Closed)         → break
      Err(other)          → warn + continue
```

Each accepted connection gets its own Tokio task.

### 5.7 Idle-Shutdown Watcher (Optional)

When `idle_shutdown_secs > 0`:

```
tokio::spawn:
  loop:
    wait for conn_count_watch to change
    if count == 0:
      debounce: sleep idle_secs, but cancel on any count change
      if still 0 after idle_secs: notify shutdown
```

The watcher tracks *connection count*, not data throughput.

### 5.8 Control Socket Server (Daemon Mode Only)

```
tokio::spawn(serve_control_socket(ControlSocketParams { ... }))
```

The control socket (`$XDG_RUNTIME_DIR/kmux/control`) serves one JSON
request/response per connection:

| Command    | Response                                                  |
|------------|-----------------------------------------------------------|
| `status`   | port, tcp_port, token, pid, uptime_secs, session_count, endpoints |
| `stop`     | `{"status":"ok"}` then triggers shutdown                  |
| `sessions` | full per-session, per-connection snapshot with metrics    |

A `SocketGuard` RAII struct removes the socket file and PID file on task exit.

The control socket task also installs its own SIGINT/SIGTERM handlers that
`notify_waiters()` on the shared `Arc<Notify> shutdown`.

### 5.9 Signal Handlers and Main Wait

```rust
tokio::select! {
    _ = ctrl_c()        => { /* SIGINT  */ }
    _ = sigterm.recv()  => { /* SIGTERM */ }
    _ = shutdown.notified() => { /* control socket "stop" or idle timeout */ }
}
```

Any of the three fires the shutdown path.

---

## 6. Steady State: Connection Dispatch

`dispatch_session(session, app)` routes each new connection to the correct
transport handler based on `session.kind`:

```
QUIC  → connection::handle_with_io(read, write, quinn_conn, app, ...)
TCP/TLS/UDS → tcp_listener::handle_tcp_io(read, write, app, kind, ...)
```

Both paths create a transport-specific `PaneAttacher` and then call the shared
`client_handler::session::run_client_session`.

---

## 7. Client Session Lifecycle (`client_handler/session.rs`)

`run_client_session` runs the full per-connection lifecycle:

### 7.1 Spawn Supporting Tasks

Three background tasks are started:

```
writer_task:  ctrl_rx → encode → write_frame (tracks bytes_out, msgs_out)
event_task:   app.subscribe_events() → pty_event_to_msg → ctrl_tx
ping_task:    every 5 s: send Ping{seq}, record send time for RTT
```

All three tasks are instrumented with the per-connection tracing span.

### 7.2 Read-Dispatch Loop

```
loop:
  data = read_frame(&mut reader)
    Some(data) → decode_client(data) → handle_message(state, msg, attacher)
                  returns false → break
    None       → debug "control stream closed" → break
    Err        → warn → break
```

Inbound byte/message counters and `last_activity_ms` are updated on every
frame, before auth.

### 7.3 Connection Teardown

```
event_task.abort()
ping_task.abort()
app.detach_client_all(client_id)     // remove from all pane ClientMaps
app.unregister_client(conn_id)       // decrement conn_count_watch
writer_task.abort()
log "connection closed"
```

---

## 8. Authentication (`client_handler/dispatch.rs`)

The first message on every connection must be `ClientMessage::Auth`.  Any
other message before auth receives no response and the connection is closed.

```
Auth { token, protocol_version, capabilities, connection_id }:
  1. protocol_version != PROTOCOL_VERSION
       → AuthResult { success: false, reason: "version mismatch" }
       → return false (close)
  2. !validate_token(token, app.auth_token)   // constant-time compare
       → AuthResult { success: false, reason: "invalid token" }
       → return true (keep reading; client may retry)
  3. Success:
       (client_id, conn_id, metrics) = app.register_client(transport, metrics, connection_id)
       state.authenticated = true
       AuthResult { success: true, client_id, connection_id, server_version }
```

`connection_id` in the `Auth` message supports **channel switching**: passing
the `ConnectionId` from a previous session causes `register_client` to reuse
the existing metrics rather than allocating a new `ConnectionId`.

---

## 9. Session and Pane Management (`app/crud.rs`, `app/pane_crud.rs`)

### 9.1 Session Creation

```
create_session(name, cwd, program, args, size, capabilities):
  - Enforce MAX_SESSIONS = 1000
  - Draw unique word_id from WordlistSampler
  - Resolve display_name (name || basename(cwd) || word_id)
  - Allocate session_index (monotonic AtomicU32)
  - Spawn initial pane (pane_index 0)
  - Insert SessionState { meta, panes, next_pane_index } into sessions RwLock
  - Return SessionEntry
```

### 9.2 Pane Creation

```
spawn_pane_relay(pane_id, program, args, size, cwd, capabilities):
  - manager.spawn(pane_id, PtyConfig { program, args, size, cwd })
  - session.split() → (PtyReader, PtyWriter)
  - Create PaneRelay:
      clients: Arc<Mutex<ClientMap>>       // empty; filled on Attach
      writer:  PtyWriter
      term_state: Arc<Mutex<TermState>>   // libghostty-vt VT emulator
      scrollback: Arc<Mutex<DiffBuffer>>  // 10 MB ring buffer of diffs
      seqno_counter: Arc<AtomicU64>       // starts at 1
      kitty_graphics_enabled / kitty_keyboard_enabled: Arc<AtomicBool>
  - tokio::spawn(session_diff_loop(...)) → stored as relay._task
```

### 9.3 PTY Output Relay (`relay.rs`)

`session_diff_loop` runs for the lifetime of each pane:

```
loop:
  n = reader.read(buf, 65536)
  ts.feed(buf[..n])
  // coalesce: drain all immediately-available bytes before diffing
  loop: ts.feed(buf[..try_read()])  until WouldBlock
  flush_cell_diff(pane_id, term_state, scrollback, clients, seqno_counter, ...)
    - compute diff vs previous cursor/modes snapshot
    - seqno = seqno_counter.fetch_add(1)
    - store (seqno, diff) in DiffBuffer
    - for each client in ClientMap:
        send TerminalUpdate via ctrl_tx (unbounded; never dropped)
        also send via data_tx (bounded; dropped if full)
  log cycle timing at DEBUG
```

Burst output (e.g. large `cat`) is coalesced into a single diff per read
cycle, reducing redundant intermediate updates to clients.

---

## 10. Persistence / Checkpointing (`persist/`, `app/persistence.rs`)

### 10.1 `checkpoint_state()`

Called every 30 seconds and at shutdown:

```
For each session (sorted by index):
  For each pane (sorted by pane_index):
    grid, scrollback_lines = term_state.snapshot() + read_history_lines()
      (scrollback capped at MAX_SCROLLBACK_LINES = 10,000)
    child_pid = manager.child_pid(pane_id)   // informational only
    PersistedPane { pane_index, program, args, size, status, child_pid, grid, scrollback_lines, cwd }
  PersistedSession { meta, next_pane_index, panes }
PersistedDaemonState { version=2, session_index_counter, sessions, used_words }
```

### 10.2 `write_checkpoint(state, path)`

```
bytes = postcard::to_allocvec(state)
write bytes → path.with_extension("bin.tmp")
rename "bin.tmp" → "bin"          // atomic; crash-safe
```

The atomic rename ensures a crash during the write never leaves a corrupt file.

### 10.3 Schema Versioning

```rust
const STATE_VERSION: u32 = 2;
```

On read, the `version` field (first varint) is peeked before full
deserialization:

| Version | Action                                          |
|---------|-------------------------------------------------|
| 1       | Deserialize as v1, migrate: add `args: vec![]`  |
| 2       | Deserialize as current                          |
| > 2     | Error: "checkpoint is from a newer daemon"      |

---

## 11. Session Restore on Startup (`app/restore.rs`, `persist/restore.rs`)

```
read_checkpoint(session_state_path) → PersistedDaemonState
app.restore_from(state):
  session_index_counter.fetch_max(checkpoint value)
  Reserve all used_words in WordlistSampler

  For each persisted session:
    For each persisted pane:
      manager.spawn(pane_id, PtyConfig { program, args, size, cwd })
      Create fresh TermState + DiffBuffer
      snapshot_to_ansi(grid, scrollback_lines)   // render old state as ANSI
      seed_pane_with_preamble(term_state, ...)   // feed ANSI into emulator
        (the client sees old scrollback above a visual separator)
      tokio::spawn(session_diff_loop(...))
    Insert SessionState → sessions map
  Return RestoreReport { restored, alive, dead }
```

The restore spawns a **fresh shell** for every pane, using the same program
and arguments as the original.  The previous terminal grid is injected into
the emulator as ANSI bytes so clients see the old visual state in the
scrollback buffer when they first attach.

Pane construction is factored into `build_pane_relay(pane_id, persisted, reader,
writer, seed: SeedMode)`. A cold start / fallback restore uses
`SeedMode::Respawned` (fresh shell + ANSI replay **with** a "[kmux: session
restored]" separator). A **graceful handoff** successor instead inherits the
predecessor's *live* PTY fd per pane and uses `SeedMode::Inherited` (same seed,
**no** separator — the live process simply continues): see
`restore_with_handoff` and `docs/daemon-handoff.md`. Panes whose child already
exited fall back to the respawn path.

---

## 12. Client Attachment and Scrollback Replay (`app/attach.rs`)

```
Attach { pane_id, last_seqno, size }:
  If already attached: abort old output task
  (client_tx, client_rx) = mpsc bounded channel
  AttachResult = app.attach(AttachParams { pane_id, client_id, last_seqno, size })
    None (never attached):          FullSnapshot { grid, seqno }
    last_seqno in DiffBuffer:       Delta { [(seqno, diff), ...] }
    last_seqno too old / scrollback overflow: SyncReset { full_grid, seqno }
  build_attach_replay(attach_result) → Vec<ServerMessage>
  attacher.start_pane_stream(pane_id, replay_msgs, client_rx)
    QUIC:  opens uni-stream, sends replay + live updates
    TCP:   sends via shared ctrl_tx
```

The three sync modes ensure a client can always reach a consistent state
regardless of how much it missed during a reconnect.

---

## 13. probe-or-start Protocol

Used by the `kmux` client over SSH to discover or start a daemon on the remote
host.

```
ssh user@host kmuxd probe-or-start

Output JSON (stdout):
{
  "protocol_version": 1,
  "kmuxd_version": "0.x.y",
  "quic_port": 8443,
  "tcp_port": 8444,
  "token": "<64-hex-char token>",
  "endpoints": [
    { "kind": "QUIC",   "address": "host:8443" },
    { "kind": "TcpTls", "address": "host:8444" }
  ]
}
```

The sequence in `main::probe_or_start()`:

1. Query control socket (2-second timeout).
2. Verify reported PID is alive via `kill(pid, 0)`.
3. If responsive → print JSON, exit 0.
4. If not → `cleanup_and_start_daemon()`:
   - Read stale PID file → SIGTERM → wait 300 ms → SIGKILL if still alive.
   - Remove stale PID file and socket.
   - `std::process::Command::new(exe).args(DAEMON_BOOT_ARGS).spawn()`.
5. Poll control socket every 200 ms for up to 10 seconds.
6. Print JSON, exit 0; or exit 1 with "timed out" on stderr.

---

## 14. Graceful Shutdown

Triggered by SIGINT, SIGTERM, or `stop` via the control socket:

```
1. Abort all listener accept loop JoinHandles
2. app.checkpoint_state().await → write_checkpoint (shutdown checkpoint)
3. endpoint.close(0u32, b"shutdown") for each QUIC endpoint
4. async_main returns Ok(())
5. Process exits; SocketGuard drop removes control socket + PID file
```

Client connections in progress are dropped ungracefully (no graceful disconnect
message is sent).

### 14.1 Graceful restart with live PTY handoff

`restart` via the control socket takes a **distinct** path (`handoff::sender`):
the daemon spawns a successor and streams each pane's live PTY master fd to it
over `handoff.sock` (`SCM_RIGHTS`), so running shells survive the restart.
Instead of the abort→checkpoint→exit sequence above, the outgoing daemon
quiesces its relays *after* the successor confirms it holds every fd, writes a
post-quiesce checkpoint, releases its sockets, and exits; the successor adopts
the auth token and rebuilds each pane around the inherited fd. On any failure it
rolls back (or the successor falls back to snapshot restore). Full sequence,
versioning, and fault-tolerance model: `docs/daemon-handoff.md`.

---

## 15. Runtime Directory Layout

All paths resolve via `kmux_protocol::dirs`:

```
$XDG_RUNTIME_DIR/kmux/
  daemon.pid          PID of the running daemon
  daemon.sock         Unix control socket (status/stop/restart/sessions)
  daemon-data.sock    Data socket (UDS listener for local clients)
  handoff.sock        Transient: live-PTY fd transfer during a graceful restart
  token               Auth token (0600)
  session_state.bin   Periodic/shutdown checkpoint (postcard format)
  session_state.bin.tmp  Transient during atomic write
```

Debug builds use a separate runtime dir to avoid colliding with release
daemons.

---

## 16. Key Data Structures

| Structure           | Location              | Role |
|---------------------|-----------------------|------|
| `ServerApp`         | `app/mod.rs`          | Central shared state: sessions, clients, wordlist, token, counters |
| `SessionState`      | `app/mod.rs`          | Per-session: metadata, panes map, `next_pane_index` |
| `PaneRelay`         | `app/mod.rs`          | Per-pane: PTY writer, client map, VT state, scrollback, diff task |
| `ClientSender`      | `app/mod.rs`          | Per-client: bounded data channel + unbounded control channel |
| `ClientMap`         | `app/mod.rs`          | `Arc<Mutex<HashMap<ClientId, ClientSender>>>` per pane |
| `SharedClientState` | `client_handler/mod.rs` | Per-connection: auth flag, client_id, conn_id, capabilities |
| `ConnectionMetrics` | `app/mod.rs`          | Per-connection: bytes/msgs in+out, RTT, last_activity_ms |
| `TermState`         | `term_state.rs`       | Server-side VT emulator (libghostty-vt backend) |
| `DiffBuffer`        | `scrollback.rs`       | Ring buffer of `(seqno, TerminalDiff)` bounded by byte capacity |
| `WordlistSampler`   | `wordlist.rs`         | Draws unique session IDs; tracks reserved words across restarts |
| `PersistedDaemonState` | `persist/mod.rs`   | Serialized checkpoint (postcard, versioned) |

---

## 17. Lifecycle State Diagram

```
main()
  │
  ├─ probe-or-start / print-config ──────► exit
  │
  ├─ [--daemon] daemonize_process()        ← BEFORE tokio
  │
  ├─ init tracing
  ├─ load config
  ├─ tokio::runtime + async_main()
  │     │
  │     ├─ load TLS material
  │     ├─ generate + persist auth token
  │     ├─ create ServerApp
  │     ├─ [checkpoint exists] restore_from()
  │     ├─ spawn: periodic_checkpoint (30 s)
  │     ├─ bind listeners (QUIC / TCP+TLS / UDS)
  │     ├─ spawn: accept loop × N listeners
  │     ├─ [idle_shutdown] spawn: idle watcher
  │     ├─ [--daemon] spawn: control socket server
  │     ├─ install SIGINT / SIGTERM
  │     │
  │     └─ STEADY STATE ◄──────────────────────────────────────┐
  │           │                                                  │
  │           ├─ accept connection                               │
  │           │   └─ dispatch_session                            │
  │           │       └─ run_client_session                      │
  │           │           ├─ spawn: writer_task                  │
  │           │           ├─ spawn: event_task                   │
  │           │           ├─ spawn: ping_task (5 s)              │
  │           │           ├─ read-dispatch loop                  │
  │           │           │   ├─ Auth → register_client          │
  │           │           │   ├─ SessionCreate → spawn shell     │
  │           │           │   ├─ Attach → replay + live stream   │
  │           │           │   ├─ PtyInput → PTY write            │
  │           │           │   └─ ... (all other messages)        │
  │           │           └─ teardown: detach + unregister       │
  │           │                                                   │
  │           ├─ PTY output (per pane, continuous)               │
  │           │   └─ session_diff_loop                           │
  │           │       ├─ read + coalesce PTY bytes               │
  │           │       ├─ feed TermState                          │
  │           │       ├─ compute diff                            │
  │           │       └─ broadcast TerminalUpdate to clients ────┘
  │           │
  │           └─ periodic checkpoint (30 s)
  │
  ├─ SIGINT / SIGTERM / control "stop" / idle timeout
  │
  └─ SHUTDOWN
      ├─ abort listener tasks
      ├─ checkpoint_state() + write_checkpoint()
      ├─ QUIC endpoint.close()
      └─ exit (SocketGuard removes control socket + PID file)
```

---

## 18. Improvement Opportunities

The following issues were identified during this analysis.  They are ordered
roughly by severity.

### 18.1 `RestoreReport.alive` / `.dead` are always zero

`app/restore.rs` increments only `report.restored`.  The `alive` and `dead`
fields exist in `RestoreReport` but are never set.  The log line at startup
always reads `alive=0 dead=0`, which is misleading.

**Fix:** After spawning each fresh shell, check whether the original
`child_pid` is still alive via `kill(pid, 0)`.  Increment `alive` or `dead`
accordingly, and use this to decide whether to emit a "session was alive /
dead at checkpoint" log.

### 18.2 Shutdown does not drain in-flight client connections

`async_main` aborts accept loop tasks but does not wait for open client
sessions to finish.  Active sessions (e.g. mid-Attach) are dropped
ungracefully.  No `SessionClosed` or disconnect message is sent to clients,
which means the client may sit in a half-open state until its own timeout
fires.

**Fix:** Broadcast a `Shutdown` server message to all connected clients via
`ctrl_tx` before aborting, then wait up to a configurable grace period (e.g.
2 seconds) for sessions to drain before force-exiting.  Use a `CancellationToken`
or `Notify` to signal all `run_client_session` loops.

### 18.3 Token printed to stdout unconditionally

`startup.rs` always `println!("Auth token: {token}")` even in daemon mode
where stdout is redirected to `/dev/null`.  The only reliable way to retrieve
the token after daemonization is to read the file at
`$XDG_RUNTIME_DIR/kmux/token`.

The `println!` is a vestige of an earlier design and creates noise in
non-SSH flows (e.g. systemd-managed daemons).

**Fix:** Only print to stdout in foreground (non-daemon) mode.  Document the
token-file path as the canonical way to retrieve the token after daemonization.

### 18.4 Checkpoint does not persist pane CWD per-pane

`PersistedPane.cwd` is set from `session_state.meta.cwd` (the session-level
CWD) rather than the live working directory of the child process.  If the user
has `cd`-ed inside the shell, the restored pane will start in the wrong
directory.

**Fix:** Query `/proc/<pid>/cwd` (Linux) or `F_GETPATH` (macOS) on the child
PID at checkpoint time and persist the result.  Fall back to the session CWD
if the query fails.

### 18.5 `idle_shutdown_secs` tracks connection count, not activity

The idle-shutdown watcher fires when no clients are *connected*, regardless of
whether any sessions contain live activity (e.g. a long-running process).  A
daemon with no attached clients but running background jobs will shut down
and lose those jobs.

**Fix:** Add an opt-in `idle_shutdown_mode` config key (`connections` vs
`activity`).  In `activity` mode, also monitor `last_activity_ms` across all
connections and `seqno_counter` across all panes, and suppress shutdown if any
pane has produced output recently.

### 18.6 Scrollback DiffBuffer uses byte capacity, not line count

`DiffBuffer` is bounded by byte capacity (10 MB), which is appropriate for
memory management.  However, there is a mismatch with the persist layer, which
caps restore at 10,000 *lines*.  A pane with long lines may persist far fewer
than 10,000 lines' worth of diffs while still hitting the byte cap in memory.

**Fix:** Align the two limits or document the intentional separation clearly.
Consider making `MAX_SCROLLBACK_LINES` configurable in `kmuxd.toml`.

### 18.7 No `listen` error is fatal — silent degradation

If a listener fails to bind (e.g. port already in use), `startup.rs` logs a
warning and continues.  The daemon may start with zero listeners if all fail,
accepting no connections and silently doing nothing.

**Fix:** After attempting all listeners, assert `!bound_listeners.is_empty()`
and return an error if zero listeners bound successfully.  Consider making
individual listener failures fatal when only one listener is configured.

### 18.8 Channel-switch metrics mismatch

`register_client` finds an existing `ConnectionId` on channel switch and
returns the *old* `ConnectionMetrics`.  The new connection's
`Arc<ConnectionMetrics>` (created in `run_client_session`) is discarded.
RTT measurements made on the new transport are attributed to an `Arc` that
nothing holds strongly; the old Arc continues to be updated by the ping task on
the new connection.

This is functionally correct (counters accumulate continuously) but the new
connection's `Arc` is wasted.  The `metrics` argument to `register_client` on
a channel-switch path is ignored.

**Fix:** `register_client` should return a `bool: was_reuse` flag.  When
`true`, `run_client_session` should drop its local `metrics` and re-clone the
one returned by `register_client`.

### 18.9 `probe-or-start` SIGKILL race on slow shutdown

`cleanup_and_start_daemon` sends SIGTERM, sleeps 300 ms, then unconditionally
removes the PID file before spawning the new daemon.  If the old daemon is
mid-checkpoint and takes longer than 300 ms to flush, SIGKILL races with the
atomic rename in `write_checkpoint`, potentially leaving a `.bin.tmp` orphan
and losing the checkpoint.

**Fix:** After SIGKILL, wait briefly for the process table entry to disappear
(poll `kill(pid, 0)`) before removing the PID file and spawning the new daemon.

### 18.10 No per-pane resource limits

There is no cap on the number of clients that can attach to a single pane, the
number of panes per session, or the rate of PTY input forwarded to a pane.
A malicious or misbehaving client can force unbounded memory growth (e.g. by
attaching many times without ever reading) or starve other clients by flooding
a pane's PTY writer.

**Fix:** Add configurable limits: `max_panes_per_session`, `max_clients_per_pane`,
and an input rate-limiter on `PtyInput` / `PtyPaste` dispatch.
