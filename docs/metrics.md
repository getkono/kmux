# Client-side metrics

kmux's client tracks four things, in-process and across restarts:

- **Render health**: net+apply latency, batch sizes, resync / lag counters
  (pre-existing, surfaced in the HUD at `Ctrl+G h`).
- **Per-(transport, category) traffic**: bytes and messages bucketed by the
  currently active transport (`UDS`, `QUIC`, `TCP+TLS`) **and** by the logical
  category of the message (Shell, Scrollback, Liveness, Control, Sync, Bootstrap).
- **Round-trip time**: per-transport EWMA plus a short rolling history, fed
  by `ClientMessage::Ping` → `ServerMessage::Pong` on a 5-second cadence.
- **Rolling history**: a JSONL file shared across every concurrent `kmux`
  process so an operator can correlate behaviour after the fact.

Everything lives under
[`crates/kmux-client/src/metrics/`](../crates/kmux-client/src/metrics/).

## Module layout

```
crates/kmux-client/src/metrics/
  mod.rs        — MetricsStore: single facade held by SessionManager
  render.rs     — RenderMetrics: apply-time stats + DiagCounters (pre-existing)
  network.rs    — NetworkMetrics: per-(TransportKey, MessageCategory) byte/msg counters
  rtt.rs        — RttTracker: per-transport EWMA + RingBuffer of recent samples
  jsonl.rs      — JsonlSink: flock-guarded append/read; 10 MiB one-gen rotation
```

`MetricsStore` composes the four collectors so `SessionManager` only needs
one field. Every existing `record_*` call on the old `metrics.rs` shim
still works — they delegate into `RenderMetrics`.

## Message categories

`MessageCategory` is defined in `crates/kmux-protocol/src/messages/category.rs`
and re-exported from `kmux_protocol::messages`. Both `ClientMessage` and
`ServerMessage` expose an exhaustive `category() -> MessageCategory` method
(no wildcard arm — adding a new protocol variant is a compile error until
categorised).

| Category    | Client messages                                       | Server messages                                                          |
|-------------|-------------------------------------------------------|--------------------------------------------------------------------------|
| `Shell`     | `PtyInput`, `PtyPaste`                                | `TerminalUpdate`, `TerminalSnapshot`, `CursorUpdate`                     |
| `Scrollback`| `FetchHistory`                                        | `HistoryLines`, `ScrollbackAppend`                                       |
| `Liveness`  | `Ping`, `Pong`                                        | `Ping`, `Pong`                                                           |
| `Control`   | Session/pane CRUD, `Attach`/`Detach`, `Resize`, `Signal`, input-lock | Session/pane replies, `Event`, `Error`, input-lock replies |
| `Sync`      | —                                                     | `Lagged`, `SyncReset`                                                    |
| `Bootstrap` | `Auth`, `ChannelReady`                                | `AuthResult`, `ChannelSwitched`                                          |

## Instrumentation points

| Where | What it calls | Category source |
|---|---|---|
| `SessionManager::send_ws` | `metrics.record_outbound(bytes, msg.category())` | `ClientMessage::category()` called on the typed message before the send. |
| `SessionManager::handle_server_message` (top) | `metrics.record_inbound(bytes, msg.category())` | `ServerMessage::category()` called after decode, before dispatch. |
| `connect::connect` success, `apply_transport_upgrade`, etc. | `metrics.on_transport_active(kind, "host:port")` | Attributes subsequent counters to the live transport. |
| `ServerMessage::Pong { seq }` | `liveness.on_pong` → RTT → `metrics.observe_rtt` + `record_rtt_to_supervisor` | Single RTT source. |

## Transport scorer integration

(Unchanged from the previous design — see `supervisor.rs`.)

## Persistence: the rolling JSONL sink

Path: `$XDG_STATE_HOME/kmux/metrics.jsonl`
(via `kmux_protocol::dirs::metrics_log_path()`).

Every 10 seconds (`METRICS_FLUSH_TICK` in `app/event_loop.rs`) the session
calls `MetricsStore::flush_sample(conn_id)`, which:

1. Calls `NetworkMetrics::take_deltas_for_active()` — returns one entry per
   `(TransportKey, MessageCategory)` bucket with a **non-zero delta** since
   the previous flush. If nothing moved, the flush is a no-op (no file write).
2. For each non-zero bucket: opens the JSONL file with `O_CREATE | O_APPEND`,
   grabs `LOCK_EX` via `nix::fcntl::flock`, rotates if the file is over 10 MiB,
   writes one `serde_json::to_string(&Sample)?\n`, and releases. The lock is held
   for microseconds.
3. Falls back silently (logging at `warn`) if any write fails.

A typical shell-heavy session emits 2–4 rows per tick (Shell + Liveness +
occasional Control). At the default flush cadence this is well below the
rotation threshold.

### Sample schema (v2)

```json
{"schema":2,"ts_ms":1713287400123,"pid":52831,"conn_id":7,
 "transport":"QUIC","endpoint":"1.2.3.4:8443","category":"Shell",
 "bytes_in":4096,"bytes_out":120,"msgs_in":8,"msgs_out":5,
 "rtt_ewma_ms":12.3,"rtt_recent_max_ms":47.0,
 "net_apply_avg_ms":3.2,"net_apply_max_ms":14.5}
{"schema":2,"ts_ms":1713287400123,"pid":52831,"conn_id":7,
 "transport":"QUIC","endpoint":"1.2.3.4:8443","category":"Liveness",
 "bytes_in":16,"bytes_out":16,"msgs_in":1,"msgs_out":1,
 "rtt_ewma_ms":12.3,"rtt_recent_max_ms":47.0,
 "net_apply_avg_ms":3.2,"net_apply_max_ms":14.5}
```

Fields are versioned via `schema`. `rtt_*` are optional (absent when no RTT
samples exist). v1 rows (no `category` field) in an existing file are silently
skipped by `read_history` (they fail to parse against the current `Sample`
struct) and are aged out naturally when the file rotates.

### One-generation rotation

At 10 MiB the active file is renamed to `metrics.jsonl.1`, overwriting
any previous generation.

## Metrics overlay

Toggle: `Ctrl+G` then `m`. The overlay shows:

- process identity (pid, connection id) and sink path (or "disabled");
- one card per observed transport, with the active one marked `●` —
  aggregate bytes in/out and msgs in/out, followed by a sub-row per
  non-zero category (Shell, Scrollback, Liveness, Control, Sync, Bootstrap)
  in stable display order, and the RTT EWMA + recent avg / max;
- apply-side render stats (identical numbers to the HUD) and diag
  counters (stale discards, seqno gaps, lag, resyncs, and the `Tear`
  tearing counter — see [`debugging-tearing.md`](debugging-tearing.md)).

Source: GTK HUD + metrics dialog in
[`crates/kmux-gtk/src/imp/dialogs.rs`](../crates/kmux-gtk/src/imp/dialogs.rs).

## Out of scope

- Per-pane bytes/msgs breakdown.
- Exporting `tracing` spans into the JSONL (separate issue).
- Prometheus / OTEL exporters.
- UI sparklines.
- Instrumenting the headless `kmux list` subcommand (bypasses SessionManager).
