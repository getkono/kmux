//! Cross-session process sampler for the process overview (issue #122).
//!
//! Each pane is backed by a PTY child (the shell); the overview shows that
//! shell plus all of its descendants, with per-process CPU and memory. The
//! daemon is the only place that knows the pane → child-PID mapping, so the
//! sampling lives here.
//!
//! The expensive part — scanning the OS process table via [`sysinfo`] — is kept
//! behind [`ProcessSampler`], which holds a warmed `System` and refreshes it
//! *lazily* (only when a request arrives and enough time has passed for CPU
//! deltas to be meaningful). An idle daemon with nobody watching the overview
//! therefore pays nothing. The pure tree-building step is factored into
//! [`build_pane_trees`] so it can be unit-tested without touching the OS.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use kmux_protocol::messages::{PaneId, PaneProcesses, ProcessSample};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Minimum spacing between `sysinfo` refreshes. CPU usage is computed as a delta
/// between successive refreshes, so refreshing faster than this yields
/// meaningless (near-zero) numbers. The client polls at ~1 Hz while the overview
/// is open, so consecutive refreshes land naturally ~1 s apart; the first sample
/// after an idle period reports 0% CPU and the next is accurate.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_millis(900);

/// Upper bound on the command line shipped per process, so a pathological argv
/// can't bloat the frame. Truncated on a char boundary.
const MAX_CMD_LEN: usize = 512;

/// A flattened process-table row, decoupled from `sysinfo` types so the
/// tree-building logic is testable in isolation.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: i32,
    pub ppid: Option<i32>,
    pub name: String,
    pub cmd: String,
    pub cpu_percent: f32,
    pub mem_bytes: u64,
}

/// Build one [`PaneProcesses`] per `(pane_id, root_pid)` root by walking the
/// process table's parent links: the subtree of every process reachable from
/// `root_pid` (the pane's shell). The subtree root reports `ppid = None` (its
/// real parent lives outside the pane), so clients can identify it.
///
/// Pure and deterministic (processes sorted by pid) for testing. A `root_pid`
/// absent from `table` (race: the shell just exited) yields an empty tree.
pub fn build_pane_trees(
    table: &HashMap<i32, ProcRow>,
    roots: &[(PaneId, Option<i32>)],
) -> Vec<PaneProcesses> {
    // Parent → children adjacency, built once for all roots.
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for row in table.values() {
        if let Some(ppid) = row.ppid {
            children.entry(ppid).or_default().push(row.pid);
        }
    }

    roots
        .iter()
        .map(|(pane_id, root_pid)| {
            let mut processes = Vec::new();
            if let Some(root) = *root_pid {
                let mut seen = HashSet::new();
                let mut stack = vec![root];
                while let Some(pid) = stack.pop() {
                    if !seen.insert(pid) {
                        continue;
                    }
                    let Some(row) = table.get(&pid) else { continue };
                    // The subtree root's parent is outside the pane: report None.
                    let ppid = if pid == root { None } else { row.ppid };
                    processes.push(ProcessSample {
                        pid,
                        ppid,
                        name: row.name.clone(),
                        cmd: row.cmd.clone(),
                        cpu_percent: row.cpu_percent,
                        mem_bytes: row.mem_bytes,
                    });
                    if let Some(kids) = children.get(&pid) {
                        stack.extend(kids.iter().copied());
                    }
                }
                processes.sort_by_key(|p| p.pid);
            }
            PaneProcesses {
                pane_id: pane_id.clone(),
                root_pid: *root_pid,
                processes,
            }
        })
        .collect()
}

/// Owns a warmed `sysinfo::System`, refreshed lazily so an idle daemon pays
/// nothing for the overview.
pub struct ProcessSampler {
    system: System,
    last_refresh: Option<Instant>,
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self {
            system: System::new(),
            last_refresh: None,
        }
    }

    /// Refresh the process table (if due) and build the per-pane process trees
    /// for `roots`. Blocking CPU work — call from a blocking context.
    pub fn sample(&mut self, now: Instant, roots: &[(PaneId, Option<i32>)]) -> Vec<PaneProcesses> {
        let due = self
            .last_refresh
            .is_none_or(|t| now.duration_since(t) >= MIN_REFRESH_INTERVAL);
        if due {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing()
                    .with_cpu()
                    .with_memory()
                    .with_cmd(UpdateKind::Always),
            );
            self.last_refresh = Some(now);
        }
        let table = self.snapshot_table();
        build_pane_trees(&table, roots)
    }

    /// Flatten the live `sysinfo` process table into our decoupled [`ProcRow`]s.
    fn snapshot_table(&self) -> HashMap<i32, ProcRow> {
        self.system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let pid = pid.as_u32() as i32;
                let name = process.name().to_string_lossy().into_owned();
                let mut cmd = process
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" ");
                if cmd.is_empty() {
                    cmd = name.clone();
                }
                truncate_on_char_boundary(&mut cmd, MAX_CMD_LEN);
                let row = ProcRow {
                    pid,
                    ppid: process.parent().map(|p| p.as_u32() as i32),
                    name,
                    cmd,
                    cpu_percent: process.cpu_usage(),
                    mem_bytes: process.memory(),
                };
                (pid, row)
            })
            .collect()
    }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 codepoint.
fn truncate_on_char_boundary(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, ppid: Option<i32>, name: &str) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            name: name.into(),
            cmd: name.into(),
            cpu_percent: 0.0,
            mem_bytes: 0,
        }
    }

    fn table(rows: Vec<ProcRow>) -> HashMap<i32, ProcRow> {
        rows.into_iter().map(|r| (r.pid, r)).collect()
    }

    #[test]
    fn collects_full_descendant_tree() {
        // shell(100) -> cargo(101) -> rustc(102); plus an unrelated process.
        let t = table(vec![
            row(100, Some(1), "zsh"),
            row(101, Some(100), "cargo"),
            row(102, Some(101), "rustc"),
            row(999, Some(1), "unrelated"),
        ]);
        let out = build_pane_trees(&t, &[("eagle/0".into(), Some(100))]);
        assert_eq!(out.len(), 1);
        let pids: Vec<i32> = out[0].processes.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![100, 101, 102]); // sorted, unrelated excluded
        assert_eq!(out[0].root_pid, Some(100));
    }

    #[test]
    fn subtree_root_reports_no_parent() {
        let t = table(vec![row(100, Some(1), "zsh"), row(101, Some(100), "vim")]);
        let out = build_pane_trees(&t, &[("eagle/0".into(), Some(100))]);
        let root = out[0].processes.iter().find(|p| p.pid == 100).unwrap();
        assert_eq!(root.ppid, None, "root's external parent must be hidden");
        let child = out[0].processes.iter().find(|p| p.pid == 101).unwrap();
        assert_eq!(child.ppid, Some(100));
    }

    #[test]
    fn missing_root_yields_empty_tree() {
        let t = table(vec![row(1, None, "init")]);
        let out = build_pane_trees(&t, &[("eagle/0".into(), Some(424242))]);
        assert_eq!(out.len(), 1);
        assert!(out[0].processes.is_empty());
        assert_eq!(out[0].root_pid, Some(424242));
    }

    #[test]
    fn none_root_yields_empty_tree() {
        let t = table(vec![row(1, None, "init")]);
        let out = build_pane_trees(&t, &[("eagle/1".into(), None)]);
        assert_eq!(out[0].root_pid, None);
        assert!(out[0].processes.is_empty());
    }

    #[test]
    fn handles_multiple_panes_and_shared_table() {
        let t = table(vec![
            row(100, Some(1), "zsh"),
            row(101, Some(100), "top"),
            row(200, Some(1), "bash"),
        ]);
        let out = build_pane_trees(
            &t,
            &[("eagle/0".into(), Some(100)), ("eagle/1".into(), Some(200))],
        );
        assert_eq!(out[0].processes.len(), 2);
        assert_eq!(out[1].processes.len(), 1);
        assert_eq!(out[1].processes[0].name, "bash");
    }

    #[test]
    fn truncates_long_cmd_on_char_boundary() {
        let mut s = "é".repeat(400); // 800 bytes
        truncate_on_char_boundary(&mut s, MAX_CMD_LEN);
        assert!(s.len() <= MAX_CMD_LEN);
        assert!(s.is_char_boundary(s.len()));
    }
}
