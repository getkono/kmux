# Pipeline verification: the grid-digest desync oracle

kmux sends terminal output as a stream of **cell diffs**, not raw bytes (see
[PERFORMANCE.md](PERFORMANCE.md) §1 and [architecture-frontend.md](architecture-frontend.md)).
That is fast, but a diff protocol has one dangerous failure mode: a bug anywhere
between the server's grid and the client's reconstructed grid can leave the two
**silently** out of sync — the screen looks plausible but is wrong, and nothing
reports it. This document describes the machinery that makes that class of bug
impossible to ship undetected.

## The seams

Output crosses several boundaries where its representation changes — each a place
a bug could corrupt state:

1. PTY bytes → server VT emulator (`TermState::feed`, `kmux-vt-core`)
2. server grid → `TerminalDiff` (`diff_engine`)
3. `TerminalDiff` → wire (`encode_server` + `write_frame_compressed*`, `codec.rs`)
4. wire → `TerminalDiff` (`read_frame` + `decode_server`)
5. `TerminalDiff` → client `CellGrid` (`grid::apply_diff`, `kmux-client`)
6. `CellGrid` → pixels (renderer cache)
7. attach/resume continuity (`app/attach.rs`, seqno gaps, scrollback)

## The oracle: grid equivalence

Both ends can produce a [`GridSnapshot`]: the server from its VT-authoritative
emulator (`TermState::snapshot()`), the client from its reconstructed grid
(`CellGrid::to_snapshot()`). If the diff stream is correct, those two grids are
identical at the same seqno. A single canonical digest makes that cheap to check:

- `GridSnapshot::digest()` — full digest (cells, cursor, modes, and the whole
  scrollback envelope incl. tail). Used by the offline conformance tests, which
  control both sides and so can compare tail contents exactly.
- `GridSnapshot::live_digest()` — the wire oracle's digest: visible grid, cursor,
  modes, and the scrollback *envelope counts* (`history_total`, `scrollback_base`),
  **excluding** tail contents. The server caps its snapshot tail at a fixed window
  and a client can be transiently behind during lazy `FetchHistory`, so hashing
  tail contents live would produce false mismatches; tail correctness is covered
  exhaustively by the conformance suite instead.

Both are a dependency-free, byte-stable 128-bit FNV-1a over a canonical field
walk (`crates/kmux-protocol/src/messages/vt.rs`). The server digest is always
computed from the VT-authoritative `TermState`, **never** from a `CellGrid`
mirror, so a federated pane (whose "server" is itself a `CellGrid`) cannot give a
tautological pass.

## Two layers

### 1. Offline conformance (deterministic, CI)

`crates/kmuxd/tests/grid_conformance.rs` drives a **real** ghostty VT with scripted
and seeded-random byte streams, reconstructs the screen by applying the emitted
diff stream to a `CellGrid`, and asserts `client.digest() == server.digest()`
after every frame. The server (libghostty) and client (`CellGrid::apply_diff`) are
genuinely independent implementations, so agreement is meaningful — this is what
caught the `'\0'`-vs-`' '` blank-cell desync that prompted the
`kmux-ghostty` `convert_cell` fix. `relay.rs`'s `relay_broadcast_reconstructs_*`
test extends this through the *real* broadcast path (seqno allocation,
`ScrollbackAppend` ordering incl. the reset-first rule, digest emission).

### 2. Live self-heal (production, in-band)

`ServerMessage::GridDigest { pane_id, seqno, hash }` (PROTOCOL_VERSION 36) carries
the server's `live_digest` for a seqno. It is emitted on the **data** channel,
right after the diffs it certifies, so it can never overtake them; throttled in
production (`KMUX_GRID_DIGEST_INTERVAL`, default 1-in-32; `=1` per-frame for tests).

A client verifies only when it is synced at exactly that seqno and has no pending
history fetch; on a mismatch it records `DiagEvent::DigestMismatch` (counter
`digest_mismatches`, surfaced in the diagnostics HUD) and triggers its existing
resync (`grid.clear()` + re-attach). This turns silent corruption into a detected,
self-healing, *countable* event — the conformance and e2e suites assert the count
stays zero.

## What still needs more than a content digest

The digest covers seams 2–5. It is **blind** to:

- **render bugs** (seam 6): the `CellGrid` is correct, only the pixels are wrong.
  Covered by the deterministic `kmux diagnostic` render patterns.
- **read-during-apply tears** if grid application is ever moved off the UI thread:
  a post-apply digest is green even if a painter observed a half-applied grid.
  That work must carry its own generation-seqlock invariant (see PERFORMANCE.md).
