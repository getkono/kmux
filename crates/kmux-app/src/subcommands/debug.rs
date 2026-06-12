//! Hidden `kmux debug` subcommands — internal diagnostics.
//!
//! `kmux debug tearing` is the offline ground-truth cross-check for the live
//! HUD tearing counter (issue #72). It pairs the daemon's per-diff trace with
//! the client's per-tick trace (both captured under `KMUX_FRAME_TRACE=1`),
//! reconstructs logical frames from daemon send-time gaps, and reports any
//! logical frame whose diffs were painted across more than one client tick.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use kmux_protocol::trace::{ClientTickRecord, DaemonDiffRecord, DiffKind};

use crate::cli::DebugAction;

pub async fn run_debug_command(action: DebugAction) -> anyhow::Result<()> {
    match action {
        DebugAction::Tearing {
            daemon_trace,
            client_trace,
            window_ms,
        } => {
            let daemon_path = match daemon_trace {
                Some(p) => p,
                None => kmux_protocol::dirs::daemon_trace_path()?,
            };
            let client_path = match client_trace {
                Some(p) => p,
                None => kmux_protocol::dirs::client_trace_path()?,
            };
            run_tearing_report(&daemon_path, &client_path, window_ms)
        }
    }
}

/// One logical frame that was painted across more than one client tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TornIncident {
    pane: String,
    seqno_lo: u64,
    seqno_hi: u64,
    sent_at_lo: u64,
    sent_at_hi: u64,
    /// Distinct client ticks that painted this frame's diffs (sorted).
    tick_ids: Vec<u64>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TearingReport {
    /// Logical frames reconstructed from the daemon trace.
    logical_frames: usize,
    /// Logical frames painted across ≥2 client ticks.
    torn_frames: usize,
    incidents: Vec<TornIncident>,
}

fn run_tearing_report(
    daemon_path: &Path,
    client_path: &Path,
    window_ms: u64,
) -> anyhow::Result<()> {
    let daemon = read_jsonl::<DaemonDiffRecord>(daemon_path)?;
    let client = read_jsonl::<ClientTickRecord>(client_path)?;

    if daemon.is_empty() {
        eprintln!(
            "warning: no daemon records in {} — was kmuxd run with KMUX_FRAME_TRACE=1?",
            daemon_path.display()
        );
    }
    if client.is_empty() {
        eprintln!(
            "warning: no client records in {} — was the kmux client run with KMUX_FRAME_TRACE=1?",
            client_path.display()
        );
    }

    let report = analyze(&daemon, &client, window_ms);

    let rate = if report.logical_frames == 0 {
        0.0
    } else {
        100.0 * report.torn_frames as f64 / report.logical_frames as f64
    };
    println!("kmux tearing report (window = {window_ms}ms)");
    println!("  daemon diffs:    {}", daemon.len());
    println!("  client ticks:    {}", client.len());
    println!("  logical frames:  {}", report.logical_frames);
    println!("  torn frames:     {} ({rate:.1}%)", report.torn_frames);
    if report.incidents.is_empty() {
        println!("  → no tearing detected.");
    } else {
        println!("  incidents (frame painted across multiple ticks):");
        for inc in &report.incidents {
            println!(
                "    pane {} seqno {}..={} (sent {}..={}ms) painted in ticks {:?}",
                inc.pane, inc.seqno_lo, inc.seqno_hi, inc.sent_at_lo, inc.sent_at_hi, inc.tick_ids
            );
        }
    }
    Ok(())
}

/// Read a JSONL file into a `Vec<T>`, skipping blank/malformed lines.
fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.is_empty() {
                None
            } else {
                serde_json::from_str::<T>(t).ok()
            }
        })
        .collect())
}

/// Pure analysis: reconstruct logical frames from the daemon `Update` records
/// (grouping per pane by send-time gap < `window_ms`) and flag any frame whose
/// constituent seqnos were painted across more than one client tick.
fn analyze(
    daemon: &[DaemonDiffRecord],
    client: &[ClientTickRecord],
    window_ms: u64,
) -> TearingReport {
    // seqno → the first client tick that painted it.
    let mut painted_tick: HashMap<u64, u64> = HashMap::new();
    for tick in client {
        if !tick.painted {
            continue;
        }
        for a in &tick.applied {
            painted_tick.entry(a.seqno).or_insert(tick.tick_id);
        }
    }

    // Cell diffs only — cursor/scrollback records are not logical-frame content.
    let mut by_pane: BTreeMap<String, Vec<&DaemonDiffRecord>> = BTreeMap::new();
    for r in daemon {
        if r.kind == DiffKind::Update {
            by_pane.entry(r.pane_id.clone()).or_default().push(r);
        }
    }

    let mut report = TearingReport::default();
    for (pane, mut recs) in by_pane {
        recs.sort_by_key(|r| (r.sent_at_ms, r.seqno));
        let mut i = 0;
        while i < recs.len() {
            // Extend the run while consecutive send-time gaps stay under window.
            let mut j = i + 1;
            while j < recs.len()
                && recs[j].sent_at_ms.saturating_sub(recs[j - 1].sent_at_ms) < window_ms
            {
                j += 1;
            }
            report.logical_frames += 1;

            let mut ticks: Vec<u64> = recs[i..j]
                .iter()
                .filter_map(|r| painted_tick.get(&r.seqno).copied())
                .collect();
            ticks.sort_unstable();
            ticks.dedup();
            if ticks.len() >= 2 {
                report.torn_frames += 1;
                report.incidents.push(TornIncident {
                    pane: pane.clone(),
                    seqno_lo: recs[i].seqno,
                    seqno_hi: recs[j - 1].seqno,
                    sent_at_lo: recs[i].sent_at_ms,
                    sent_at_hi: recs[j - 1].sent_at_ms,
                    tick_ids: ticks,
                });
            }
            i = j;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use kmux_protocol::trace::AppliedDiff;

    fn d(pane: &str, seqno: u64, sent_at_ms: u64) -> DaemonDiffRecord {
        DaemonDiffRecord {
            pane_id: pane.to_string(),
            seqno,
            sent_at_ms,
            ops: 10,
            kind: DiffKind::Update,
        }
    }

    fn tick(tick_id: u64, seqnos: &[u64]) -> ClientTickRecord {
        ClientTickRecord {
            tick_id,
            at_ms: 0,
            applied: seqnos
                .iter()
                .map(|&seqno| AppliedDiff {
                    seqno,
                    sent_at_ms: 0,
                    ops: 10,
                })
                .collect(),
            painted: true,
        }
    }

    #[test]
    fn split_frame_painted_across_two_ticks_is_torn() {
        // Two diffs 8ms apart (one logical frame), painted in separate ticks.
        let daemon = [d("p", 0, 1_000), d("p", 1, 1_008)];
        let client = [tick(1, &[0]), tick(2, &[1])];
        let r = analyze(&daemon, &client, 16);
        assert_eq!(r.logical_frames, 1);
        assert_eq!(r.torn_frames, 1);
        assert_eq!(r.incidents[0].tick_ids, vec![1, 2]);
    }

    #[test]
    fn frame_painted_in_one_tick_is_clean() {
        let daemon = [d("p", 0, 1_000), d("p", 1, 1_008)];
        let client = [tick(1, &[0, 1])];
        let r = analyze(&daemon, &client, 16);
        assert_eq!(r.logical_frames, 1);
        assert_eq!(r.torn_frames, 0);
    }

    #[test]
    fn diffs_beyond_window_are_separate_frames() {
        // 50ms apart → two logical frames, each painted in its own tick: clean.
        let daemon = [d("p", 0, 1_000), d("p", 1, 1_050)];
        let client = [tick(1, &[0]), tick(2, &[1])];
        let r = analyze(&daemon, &client, 16);
        assert_eq!(r.logical_frames, 2);
        assert_eq!(r.torn_frames, 0);
    }

    #[test]
    fn per_pane_grouping_is_independent() {
        // Same timestamps on two panes: each pane forms its own logical frame.
        let daemon = [
            d("a", 0, 1_000),
            d("b", 1, 1_004),
            d("a", 2, 1_008),
            d("b", 3, 1_010),
        ];
        // Pane a split across ticks 1,2 → torn; pane b painted together in tick 1.
        let client = [tick(1, &[0, 1, 3]), tick(2, &[2])];
        let r = analyze(&daemon, &client, 16);
        assert_eq!(r.logical_frames, 2);
        assert_eq!(r.torn_frames, 1);
        assert_eq!(r.incidents[0].pane, "a");
    }
}
