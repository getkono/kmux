# Debugging shell tearing (issue #72)

Under high-jitter links, a single logical screen paint can be torn: the daemon
emits **one diff per PTY read cycle** (no 60 Hz grouping — see
`crates/kmuxd/src/relay.rs`), and the client drains + applies whatever diffs are
queued each ~16 ms pump tick, then paints once
(`crates/kmux-app/src/driver/mod.rs`). When the two halves of one logical frame
land in **different** pump ticks, the client paints an intermediate state. This
is hard to reproduce without controllable latency, so this toolkit makes it
observable. It is **diagnostics only** — normal operation is unchanged unless a
knob is set. The fix (daemon-side ~60 Hz coalescing) is deliberately separate.

## 1. Network impairment shim (daemon)

`crates/kmuxd/src/impair.rs`. Set on `kmuxd` to delay **pane-data frames only**
(`Shell` / `Scrollback`); liveness/control frames are never delayed, so the
client's ping timeout is unaffected.

| Env var | Meaning |
|---|---|
| `KMUX_NET_DELAY_MS` | Fixed latency added to each pane-data frame. |
| `KMUX_NET_JITTER_MS` | Extra uniform-random `0..=N` ms added per frame. |
| `KMUX_NET_SEED` | Optional seed for reproducible jitter. |

Zero-cost when unset (both 0/absent ⇒ the shim is a complete no-op). The delay
is applied in the per-pane writer tasks (`connection.rs` for QUIC,
`tcp_listener.rs` for TCP+TLS), so per-pane diff ordering is preserved.

## 2. Live tearing detector (client HUD)

`crates/kmux-app/src/driver/mod.rs` (`tear_detected` + `detect_tears`). Uses the
`sent_at_ms` already carried on every diff — **no protocol change**. Each pump
tick, if the earliest qualifying cell diff applied this tick was emitted within
`KMUX_TEAR_WINDOW_MS` (default 16) of the diff painted last tick, the previous
paint showed a partial logical frame and the **`Tear:` counter** in the GTK HUD
(`Ctrl+G h`, also the `Ctrl+G m` metrics dialog) increments. Diffs below
`KMUX_TEAR_MIN_OPS` (default 4) cell ops are ignored (keystroke echoes); cursor
blinks are `CursorUpdate` and already excluded.

The live counter is an over-sensitive heuristic: a faster-than-60 Hz source
(diffs continuously < window apart) is flagged as tearing, which is faithful to
the issue's definition (such a stream *should* be coalesced and never painted
partially). The counter only reflects diffs the batch contained — during a
resync storm it can over-count; cross-check with the offline analyzer.

The metrics HUD is GTK-only today, so the counter is not surfaced in the Swift
client (it still increments in `MetricsStore`; FFI parity is future work).

## 3. Frame trace + offline analyzer (ground truth)

Set `KMUX_FRAME_TRACE=1` on **both** `kmuxd` and the client. The daemon appends
one record per emitted diff to `<state_dir>/frame_trace_daemon.jsonl`
(`crates/kmuxd/src/trace.rs`); the client appends one record per pump tick to
`<state_dir>/frame_trace_client.jsonl` (`crates/kmux-app/src/driver/frame_trace.rs`).
The record schemas are shared in `kmux_protocol::trace`. `<state_dir>` is
`$XDG_STATE_HOME/kmux/` (`kmux-debug/` for debug builds).

Analyze offline:

```
kmux debug tearing \
  --daemon-trace <state_dir>/frame_trace_daemon.jsonl \
  --client-trace <state_dir>/frame_trace_client.jsonl \
  --window-ms 16
```

(Both paths default to the state-dir locations.) The analyzer reconstructs
logical frames from daemon send-time gaps and reports any frame whose diffs were
painted across more than one client tick — the ground-truth cross-check on the
live counter.

## End-to-end repro

```
KMUX_NET_DELAY_MS=40 KMUX_NET_JITTER_MS=60 KMUX_NET_SEED=1 KMUX_FRAME_TRACE=1 \
  cargo run -p kmuxd
# in another terminal: launch the client, KMUX_FRAME_TRACE=1, toggle the HUD
# (Ctrl+G h), run a full-screen TUI (vim/htop) and watch `Tear:` climb.
KMUX_FRAME_TRACE=1 just start
# then: kmux debug tearing  (corroborate the HUD count)
```

Re-run with `KMUX_NET_DELAY_MS=0 KMUX_NET_JITTER_MS=0`: `Tear:` should stay ~0
and the analyzer report ~0 torn frames, confirming the impairment is what
induces the tearing.
