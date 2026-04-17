# Client-side metrics

kmux's client tracks four things, in-process and across restarts:

- **Render health**: net+apply latency, batch sizes, resync / lag counters
  (pre-existing, surfaced in the HUD at `Ctrl+G h`).
- **Per-transport traffic**: bytes and messages, bucketed by the currently
  active transport (`UDS`, `QUIC`, `TCP+TLS`).
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
  network.rs    — NetworkMetrics: per-(TransportKind, endpoint) byte/msg counters
  rtt.rs        — RttTracker: per-transport EWMA + RingBuffer of recent samples
  jsonl.rs      — JsonlSink: flock-guarded append/read; 10 MiB one-gen rotation
```

`MetricsStore` composes the four collectors so `SessionManager` only needs
one field. Every existing `record_*` call on the old `metrics.rs` shim
still works — they delegate into `RenderMetrics`.

## Instrumentation points

| Where | What it calls | Why |
|---|---|---|
| `SessionManager::send_ws` | `metrics.record_outbound(bytes)` | One choke-point for every `ClientMessage` sent. |
| `SessionManager::handle_server_message` (top) | `metrics.record_inbound(bytes)` | Inbound count is computed by re-encoding the decoded message; cheap, and avoids plumbing frame sizes through four connect paths. |
| `connect::connect` success, `apply_transport_upgrade`, `apply_tcp_fallback`, `apply_quic_upgrade`, `set_ws_sender` | `metrics.on_transport_active(kind, "host:port")` | Attributes subsequent counters to the live transport. |
| `ServerMessage::Pong { seq }` | `liveness.on_pong` → RTT → `metrics.observe_rtt` + `record_rtt_to_supervisor` | Single source of RTT measurement; drives both the overlay and the transport scorer's EWMA (previously hard-coded to `LATENCY_UNKNOWN_MS = 500`). |

The server doesn't need any changes: `kmuxd` already replies to
`ClientMessage::Ping` and sends its own `ServerMessage::Ping` on a separate
cadence.

## Transport scorer integration

The supervisor in
[`supervisor.rs`](../crates/kmux-client/src/supervisor.rs) owns one
`EndpointHealth` per advertised endpoint. `EndpointHealth::record_rtt`
was implemented but never called — the scorer was treating every
endpoint's latency as unknown. The session manager now forwards every
Pong-derived RTT onto an `mpsc::UnboundedSender<RttSample>` that the
supervisor selects on in `run`. If no supervisor is alive (direct QUIC
path without a fallback) the channel simply isn't hooked up and the
metrics overlay is still populated.

## Persistence: the rolling JSONL sink

Path: `$XDG_STATE_HOME/kmux/metrics.jsonl`
(via `kmux_protocol::dirs::metrics_log_path`, which sits beside the
existing `client_log_path` / `connection_log_path` helpers).

Every 10 seconds (`METRICS_FLUSH_TICK` in `app/event_loop.rs`) the session
calls `MetricsStore::flush_sample(conn_id)`, which:

1. Takes the **delta** for the currently-active transport — i.e. the diff
   since the previous flush. Concurrent `kmux` processes therefore don't
   each write cumulative totals.
2. Opens the JSONL file with `O_CREATE | O_APPEND`, grabs `LOCK_EX` via
   `nix::fcntl::flock`, rotates if the file is over 10 MiB, writes one
   `serde_json::to_string(&Sample)?\n`, and releases. The lock is held
   for microseconds, so multiple writers serialise cleanly.
3. Falls back silently (logging at `warn`) if the write fails. Metrics
   never take down the client.

Reads (`JsonlSink::read_history(limit)`) take a `LOCK_SH`, tail-decode
into a ring buffer, and skip lines that fail to parse. This keeps the
reader tolerant of partial writes from a crashed peer or schema skew.

### Sample schema

```json
{
  "schema": 1,
  "ts_ms": 1713287400123,
  "pid": 52831,
  "conn_id": 7,
  "transport": "QUIC",
  "endpoint": "1.2.3.4:8443",
  "bytes_in": 4096,
  "bytes_out": 1280,
  "msgs_in": 8,
  "msgs_out": 5,
  "rtt_ewma_ms": 12.3,
  "rtt_recent_max_ms": 47.0,
  "net_apply_avg_ms": 3.2,
  "net_apply_max_ms": 14.5
}
```

Fields are versioned via `schema`; bump it only when a consumer would
need to branch. `transport` / `endpoint` / `conn_id` / `rtt_*` are
optional (serialised as absent when missing) so early samples from a
fresh connection stay usable.

### One-generation rotation

At 10 MiB the active file is renamed to `metrics.jsonl.1`, overwriting
any previous generation. Keeping one spare file is deliberate: deeper
history belongs in a real time-series store, and the append log is
meant to be tailed live, not mined.

## Metrics overlay

Toggle: `Ctrl+G` then `m` (alongside `h` for the HUD). The overlay shows:

- process identity (pid, connection id) and sink path (or "disabled");
- one card per observed transport, with the active one marked `●` —
  bytes in/out, messages in/out, and RTT EWMA + recent avg / max;
- apply-side render stats (identical numbers to the HUD) and diag
  counters (stale discards, seqno gaps, lag, resyncs).

Source: [`crates/kmux/src/ui/overlays/metrics.rs`](../crates/kmux/src/ui/overlays/metrics.rs).
The overlay only reads in-memory state; a follow-up could add a tab that
reads the rolling JSONL via `JsonlSink::read_history` for an "all-time"
view, but that's not in today's scope.

## Out of scope

- Per-pane bytes/msgs breakdown.
- Exporting `tracing` spans into the JSONL (would let us reconstruct
  bootstrap → attach → apply timelines end-to-end; separate issue).
- Prometheus / OTEL exporters.
- UI sparklines.
