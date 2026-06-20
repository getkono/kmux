# Process overview (issue #122)

A high-level, hierarchical overview of **everything running across all sessions**
— each session's `Tab → Pane → Process` tree with per-process CPU and memory —
so you can see what's busy without opening each session. Think of it as an
Activity-Monitor scoped to kmux's existing session hierarchy.

It surfaces in three places, all driven by one shared projection:

- **GUI** — a main-area view (GTK + Swift) that takes over the terminal area when
  toggled (`Ctrl+Shift+O` on GTK, `⌘⇧O` on the Swift app; also the `/processes`
  command and the main menu). Esc / `q` close it.
- **CLI** — `kmux ps` (alias `kmux top`), the headless, hierarchical counterpart
  of `kmux ls`. `--format json` emits the raw per-pane process trees.

## Data flow

```
                       sysinfo scan (daemon, lazy)
                                │
kmuxd: pane → child PID ──► ProcessSampler.sample ──► Vec<PaneProcesses>
                                                          │  (+ federated peers,
                                                          │   pane ids localized)
                                ProcessOverviewResult ◄───┘
                                       │ (wire, postcard)
kmux-client: SessionManager.process_overview cache
                                       │
kmux-app: build_overview_rows(session_list, snapshot) → Vec<OverviewRow>
                                       │ (flat, depth-tagged)
              ┌────────────────────────┼────────────────────────┐
          kmux-gtk overview.rs    kmux-ffi overview_rows()   kmux ps (render.rs)
                                   → Swift ProcessOverviewView
```

### Daemon sampling (`crates/kmuxd/src/process_stats.rs`)

The daemon is the only place that knows the pane → PTY-child-PID mapping, so it
does the sampling. Each pane's child pid comes from the pty registry
(`registry.child_pid`); the process tree under it is built from a
[`sysinfo`](https://crates.io/crates/sysinfo) scan of the OS process table.

- **Cross-platform.** `sysinfo` gives CPU% and resident memory on both Linux and
  macOS, replacing the need for Linux-only `/proc` parsing. (The separate,
  Linux-only `foreground_process_name` poll in `relay.rs`, which drives *pane
  titles*, is left as-is; it could share this sampler later.)
- **Lazy refresh.** `ProcessSampler` holds a warmed `sysinfo::System` and only
  refreshes when a request arrives and at least `MIN_REFRESH_INTERVAL` (~900 ms)
  has elapsed. An idle daemon with nobody watching the overview therefore pays
  nothing. CPU usage is a delta between refreshes, so the **first** sample after
  an idle period reads 0% and the next (the client polls ~1 Hz while open) is
  accurate.
- **Off the async runtime.** The scan runs under `tokio::task::spawn_blocking`.
- **Pure tree-building.** `build_pane_trees(table, roots)` (the BFS-by-parent-PID
  step) is factored out of the `sysinfo` refresh so it is unit-tested without
  touching the OS.

### Federation (`crates/kmuxd/src/federation/mod.rs`)

Federated sessions live on remote daemons (issue #121), so their process trees
must be fetched live — unlike the *session list*, which the hub caches. The hub
mirrors the `create_remote_session` request/await pattern:
`PeerManager::collect_process_overview` fans a `ProcessOverview` request out to
every connected peer, registers a oneshot per peer (`pending_overviews`), and
awaits them concurrently with a short per-peer timeout (a slow/dead peer simply
contributes nothing that round). The feed loop completes each oneshot when the
matching `ProcessOverviewResult` arrives, **translating each pane id remote →
local** (via `to_local_pane`) so the client joins them against its localized
session list. The dispatch handler merges local + federated and replies once.

### Protocol (`crates/kmux-protocol`)

- `ClientMessage::ProcessOverview { request_id }` →
  `ServerMessage::ProcessOverviewResult { request_id, panes: Vec<PaneProcesses> }`
  (mirrors `SessionList`/`SessionListResult`).
- `PaneProcesses { pane_id, root_pid, processes: Vec<ProcessSample> }`;
  `ProcessSample { pid, ppid, name, cmd, cpu_percent, mem_bytes }`.
- Added in `PROTOCOL_VERSION` **28**.

### Client + projection

The client (`SessionManager`) caches the latest snapshot and exposes it via
`process_overview()`. `kmux-app` joins it with the session list in one place —
`core::build_overview_rows` — producing a **flat, depth-tagged** `Vec<OverviewRow>`
(`depth`: 0 = session, 1 = tab, 2 = pane, 3+ = nested processes). A flat
projection rather than a nested tree lets all three renderers indent on `depth`
and share identical output. CPU/memory aggregate up the tree: a pane row sums its
process subtree, a tab its panes, a session all its panes.

The driver re-requests the snapshot at `PROCESS_OVERVIEW_TICK` (~1 Hz) **only
while `Mode::ProcessOverview` is active** (`FrontendDriver::tick_process_overview`),
so polling stops the moment the view closes.

### GUI surfaces

- **GTK** (`crates/kmux-gtk/src/imp/overview.rs`): an `overview` child of the
  shell's content stack (alongside `panes`/`empty`), a `ListBox` reconciled from
  `overview_rows()` while the mode is active.
- **Swift** (`kmux-swift/Sources/KmuxApp/ProcessOverviewView.swift`): shown in the
  detail area in place of the terminal when `mode == .processOverview`. Rows come
  over the FFI as `FfiOverviewRow` (`overview_rows()` getter). The FFI surface is
  `KMUX_FFI_ABI_VERSION` **16**.

## Scope / future work

- **GPU usage is deferred.** The issue lists CPU/memory/GPU, but there is no
  cross-platform per-process GPU API (NVML covers NVIDIA on Linux; macOS exposes
  none). CPU + memory ship now; `ProcessSample` carries a `TODO(#122)` marking
  where GPU fields would attach (a future `PROTOCOL_VERSION` bump).
- Sorting/filtering, kill-from-overview, and column customization are possible
  follow-ups; the projection already carries the data they would need.
