//! Process-overview projection (issue #122).
//!
//! Joins the client's session list (Session → Tab → Pane) with the latest
//! per-pane process trees from the daemon into a single **flat, depth-tagged**
//! row list. A flat projection — rather than a nested tree — lets every frontend
//! (GTK `ColumnView`, SwiftUI `Table`, and the `kmux ps` CLI) render the same
//! shape by indenting on [`OverviewRow::depth`]. CPU and memory aggregate up the
//! tree: a pane row sums its process subtree, a tab row sums its panes, and a
//! session row sums all of its panes.

use std::collections::HashMap;

use kmux_protocol::format_pane_id;
use kmux_protocol::messages::{PaneInfo, PaneProcesses, SessionEntry};

use super::AppCore;

/// What a row in the process overview represents. Drives the indent depth and
/// lets frontends style each tier differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewRowKind {
    Session,
    Tab,
    Pane,
    Process,
}

/// One flattened row of the process overview. `depth` is the indent level
/// (0 = session, 1 = tab, 2 = pane, 3+ = nested processes).
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewRow {
    pub depth: u8,
    pub kind: OverviewRowKind,
    /// Primary label: session/tab name, pane title (or program), process name.
    pub label: String,
    /// Secondary detail: session cwd, pane id, or a process's command line.
    pub detail: String,
    /// CPU percent — aggregated (subtree sum) for session/tab/pane rows, the
    /// process's own usage for process rows. Percent of a single core
    /// (can exceed 100 across cores).
    pub cpu_percent: f32,
    /// Resident memory in bytes — aggregated like `cpu_percent`.
    pub mem_bytes: u64,
    /// PID for process rows (and the shell pid for pane rows); `None` otherwise.
    pub pid: Option<i32>,
    /// The federated peer this row belongs to (session rows only); `None` for
    /// local sessions.
    pub peer: Option<String>,
}

impl AppCore {
    /// Build the flat, depth-tagged process-overview rows (issue #122) by
    /// joining this client's session list with the latest process snapshot.
    pub fn overview_rows(&self) -> Vec<OverviewRow> {
        build_overview_rows(self.mgr.session_list(), self.mgr.process_overview())
    }
}

/// Join a session list with a per-pane process snapshot into flat,
/// depth-tagged overview rows (issue #122). Shared by [`AppCore::overview_rows`]
/// (the GUIs) and the headless `kmux ps` CLI so both render an identical tree.
///
/// Empty when there are no sessions; rows still render (with zeroed stats) for
/// panes the snapshot has no data for yet (e.g. before the first refresh, or a
/// federated peer that did not answer this round).
pub fn build_overview_rows(
    sessions: &[SessionEntry],
    snapshot: &[PaneProcesses],
) -> Vec<OverviewRow> {
    let by_pane: HashMap<&str, &PaneProcesses> =
        snapshot.iter().map(|p| (p.pane_id.as_str(), p)).collect();

    let mut rows = Vec::new();
    for entry in sessions {
        let (s_cpu, s_mem) =
            aggregate_pane_ids(entry.panes.iter().map(|p| p.pane_id.as_str()), &by_pane);
        rows.push(OverviewRow {
            depth: 0,
            kind: OverviewRowKind::Session,
            label: entry.meta.name.clone(),
            detail: entry.meta.cwd.clone(),
            cpu_percent: s_cpu,
            mem_bytes: s_mem,
            pid: None,
            peer: entry.peer.clone(),
        });

        for tab in &entry.tabs {
            let leaves = tab.layout.leaves();
            let pane_ids: Vec<String> = leaves
                .iter()
                .map(|i| format_pane_id(&entry.meta.word_id, *i))
                .collect();
            let (t_cpu, t_mem) = aggregate_pane_ids(pane_ids.iter().map(String::as_str), &by_pane);
            rows.push(OverviewRow {
                depth: 1,
                kind: OverviewRowKind::Tab,
                label: tab.name.clone(),
                detail: String::new(),
                cpu_percent: t_cpu,
                mem_bytes: t_mem,
                pid: None,
                peer: None,
            });

            for &pane_index in &leaves {
                let pane_id = format_pane_id(&entry.meta.word_id, pane_index);
                let info = entry.panes.iter().find(|p| p.pane_index == pane_index);
                let pp = by_pane.get(pane_id.as_str()).copied();
                let (p_cpu, p_mem) = pp.map_or((0.0, 0), tree_totals);
                rows.push(OverviewRow {
                    depth: 2,
                    kind: OverviewRowKind::Pane,
                    label: info.map_or_else(|| pane_id.clone(), pane_label),
                    detail: pane_id.clone(),
                    cpu_percent: p_cpu,
                    mem_bytes: p_mem,
                    pid: pp.and_then(|p| p.root_pid),
                    peer: None,
                });
                if let Some(pp) = pp {
                    push_process_rows(&mut rows, pp);
                }
            }
        }
    }
    rows
}

/// A pane's display label: its window title, else its program, else its pane id.
fn pane_label(info: &PaneInfo) -> String {
    if !info.title.is_empty() {
        info.title.clone()
    } else if !info.program.is_empty() {
        info.program.clone()
    } else {
        info.pane_id.clone()
    }
}

/// Sum (`cpu_percent`, `mem_bytes`) over a pane's process subtree.
fn tree_totals(pp: &PaneProcesses) -> (f32, u64) {
    pp.processes.iter().fold((0.0, 0), |(cpu, mem), p| {
        (cpu + p.cpu_percent, mem + p.mem_bytes)
    })
}

/// Sum the subtree totals over a set of pane ids, skipping panes the snapshot
/// has no data for.
fn aggregate_pane_ids<'a>(
    pane_ids: impl Iterator<Item = &'a str>,
    by_pane: &HashMap<&str, &PaneProcesses>,
) -> (f32, u64) {
    pane_ids.fold((0.0, 0), |(cpu, mem), id| match by_pane.get(id) {
        Some(pp) => {
            let (c, m) = tree_totals(pp);
            (cpu + c, mem + m)
        }
        None => (cpu, mem),
    })
}

/// Append the pane's process tree as nested rows (depth 3+), ordered by the
/// parent → child links the daemon reported. The subtree root(s) carry
/// `ppid == None`; children are emitted depth-first, sorted by pid for stable
/// ordering.
fn push_process_rows(rows: &mut Vec<OverviewRow>, pp: &PaneProcesses) {
    // Parent → children adjacency over this pane's processes.
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let mut roots: Vec<i32> = Vec::new();
    for proc in &pp.processes {
        match proc.ppid {
            Some(ppid) => children.entry(ppid).or_default().push(proc.pid),
            None => roots.push(proc.pid),
        }
    }
    for kids in children.values_mut() {
        kids.sort_unstable();
    }
    roots.sort_unstable();

    let by_pid: HashMap<i32, _> = pp.processes.iter().map(|p| (p.pid, p)).collect();
    // Iterative DFS carrying each node's depth (base 3 = directly under a pane).
    let mut stack: Vec<(i32, u8)> = roots.iter().rev().map(|&pid| (pid, 3)).collect();
    while let Some((pid, depth)) = stack.pop() {
        let Some(proc) = by_pid.get(&pid) else {
            continue;
        };
        rows.push(OverviewRow {
            depth,
            kind: OverviewRowKind::Process,
            label: proc.name.clone(),
            detail: proc.cmd.clone(),
            cpu_percent: proc.cpu_percent,
            mem_bytes: proc.mem_bytes,
            pid: Some(pid),
            peer: None,
        });
        if let Some(kids) = children.get(&pid) {
            // Push reversed so the smallest pid is processed first.
            for &kid in kids.iter().rev() {
                stack.push((kid, depth.saturating_add(1)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_client::session_manager::SessionManager;
    use kmux_protocol::messages::{
        ClientCapabilities, LayoutNode, PaneInfo, PaneProcesses, ProcessSample, SessionEntry,
        SessionMeta, SessionStatus, TabInfo, TermSize,
    };

    fn pane(word: &str, idx: u32) -> PaneInfo {
        PaneInfo {
            pane_id: format_pane_id(word, idx),
            pane_index: idx,
            program: "zsh".into(),
            size: TermSize::default(),
            attached_clients: vec![],
            status: SessionStatus::Running,
            title: String::new(),
            progress_state: Default::default(),
            progress: None,
        }
    }

    fn entry(word: &str, panes: u32) -> SessionEntry {
        SessionEntry {
            meta: SessionMeta {
                index: 0,
                word_id: word.into(),
                name: word.into(),
                cwd: "/proj".into(),
            },
            panes: (0..panes).map(|i| pane(word, i)).collect(),
            tabs: (0..panes)
                .map(|i| TabInfo {
                    tab_index: i,
                    name: format!("{}", i + 1),
                    layout: LayoutNode::single(i),
                    focused_pane: i,
                })
                .collect(),
            active_tab: 0,
            peer: None,
        }
    }

    fn core_with(entries: Vec<SessionEntry>, snapshot: Vec<PaneProcesses>) -> AppCore {
        let mut mgr = SessionManager::new(
            "127.0.0.1".into(),
            8443,
            "tok".into(),
            true,
            ClientCapabilities::default(),
        );
        for e in entries {
            mgr.session_list.push(e);
        }
        mgr.process_overview = snapshot;
        AppCore::for_test(mgr)
    }

    #[test]
    fn projects_full_hierarchy_with_aggregates() {
        let snapshot = vec![PaneProcesses {
            pane_id: "eagle/0".into(),
            root_pid: Some(100),
            processes: vec![
                ProcessSample {
                    pid: 100,
                    ppid: None,
                    name: "zsh".into(),
                    cmd: "-zsh".into(),
                    cpu_percent: 1.0,
                    mem_bytes: 1000,
                },
                ProcessSample {
                    pid: 101,
                    ppid: Some(100),
                    name: "cargo".into(),
                    cmd: "cargo build".into(),
                    cpu_percent: 9.0,
                    mem_bytes: 9000,
                },
            ],
        }];
        let core = core_with(vec![entry("eagle", 1)], snapshot);
        let rows = core.overview_rows();

        // Session, Tab, Pane, then 2 process rows.
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].kind, OverviewRowKind::Session);
        assert_eq!(rows[0].depth, 0);
        // Aggregates roll up the whole subtree.
        assert_eq!(rows[0].cpu_percent, 10.0);
        assert_eq!(rows[0].mem_bytes, 10_000);
        assert_eq!(rows[1].kind, OverviewRowKind::Tab);
        assert_eq!(rows[2].kind, OverviewRowKind::Pane);
        assert_eq!(rows[2].mem_bytes, 10_000);
        // Process rows nest: shell at depth 3, its child at depth 4.
        assert_eq!(rows[3].kind, OverviewRowKind::Process);
        assert_eq!(rows[3].depth, 3);
        assert_eq!(rows[3].pid, Some(100));
        assert_eq!(rows[4].depth, 4);
        assert_eq!(rows[4].pid, Some(101));
        assert_eq!(rows[4].label, "cargo");
    }

    #[test]
    fn panes_without_snapshot_data_render_with_zeros() {
        let core = core_with(vec![entry("eagle", 1)], vec![]);
        let rows = core.overview_rows();
        // Session, Tab, Pane (no process rows) — all zeroed.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].kind, OverviewRowKind::Pane);
        assert_eq!(rows[2].cpu_percent, 0.0);
        assert_eq!(rows[2].mem_bytes, 0);
        assert_eq!(rows[2].pid, None);
    }

    #[test]
    fn empty_session_list_yields_no_rows() {
        let core = core_with(vec![], vec![]);
        assert!(core.overview_rows().is_empty());
    }
}
