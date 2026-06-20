//! Process-overview types for the cross-session process monitor (issue #122).
//!
//! The daemon samples, per pane, the process tree rooted at that pane's PTY
//! child (the shell) and ships it to clients as [`PaneProcesses`]. The client
//! already knows the Session → Tab → Pane hierarchy from
//! [`SessionEntry`](super::session::SessionEntry); joining it with these
//! per-pane trees yields the full Session → Tab → Pane → Process overview.

use serde::{Deserialize, Serialize};

use super::session::PaneId;

/// A single OS process observed inside a pane's process tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessSample {
    /// OS process id.
    pub pid: i32,
    /// Parent process id, or `None` when the parent lies outside this pane's
    /// subtree (i.e. this sample is the subtree root).
    pub ppid: Option<i32>,
    /// Short process name (e.g. `"zsh"`, `"vim"`).
    pub name: String,
    /// Full command line (argv joined with spaces); may be truncated by the
    /// sampler. Empty when the OS does not expose it.
    pub cmd: String,
    /// CPU usage as a percentage of a single core (sysinfo convention: can
    /// exceed `100.0` across multiple cores). `0.0` until two samples spaced by
    /// the sampler's minimum interval have been taken.
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub mem_bytes: u64,
    // TODO(#122): add `gpu_percent` / `gpu_mem_bytes` once a per-process GPU API
    // is available (NVML on NVIDIA/Linux; macOS has no public per-process GPU
    // API). Deferred deliberately so this surface stays cross-platform; adding
    // the fields later is a `PROTOCOL_VERSION` bump.
}

/// The process tree of a single pane: the pane's PTY child (the shell) plus all
/// of its descendants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneProcesses {
    /// Which pane this tree belongs to (`"{word_id}/{pane_index}"`). For a
    /// federated pane this is the **local** (hub-assigned) pane id, so the
    /// client can join it against its localized session list.
    pub pane_id: PaneId,
    /// The pane's PTY child pid (the root of `processes`), or `None` when the
    /// child has already exited or its pid could not be read.
    pub root_pid: Option<i32>,
    /// The shell plus every descendant process, in no particular order. Clients
    /// rebuild the hierarchy from [`ProcessSample::ppid`].
    pub processes: Vec<ProcessSample>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_sample_roundtrips() {
        let sample = ProcessSample {
            pid: 4242,
            ppid: Some(1),
            name: "vim".into(),
            cmd: "vim src/main.rs".into(),
            cpu_percent: 12.5,
            mem_bytes: 34_000_000,
        };
        let bytes = postcard::to_allocvec(&sample).expect("serialize");
        let decoded: ProcessSample = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded, sample);
    }

    #[test]
    fn pane_processes_roundtrips() {
        let pane = PaneProcesses {
            pane_id: "eagle/0".into(),
            root_pid: Some(100),
            processes: vec![
                ProcessSample {
                    pid: 100,
                    ppid: None,
                    name: "zsh".into(),
                    cmd: "-zsh".into(),
                    cpu_percent: 0.0,
                    mem_bytes: 5_000_000,
                },
                ProcessSample {
                    pid: 101,
                    ppid: Some(100),
                    name: "cargo".into(),
                    cmd: "cargo build".into(),
                    cpu_percent: 95.0,
                    mem_bytes: 250_000_000,
                },
            ],
        };
        let bytes = postcard::to_allocvec(&pane).expect("serialize");
        let decoded: PaneProcesses = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded, pane);
    }

    #[test]
    fn pane_processes_empty_when_child_exited() {
        let pane = PaneProcesses {
            pane_id: "eagle/1".into(),
            root_pid: None,
            processes: vec![],
        };
        let bytes = postcard::to_allocvec(&pane).expect("serialize");
        let decoded: PaneProcesses = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(decoded, pane);
        assert!(decoded.processes.is_empty());
        assert_eq!(decoded.root_pid, None);
    }
}
