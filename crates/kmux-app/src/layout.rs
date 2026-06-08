//! Toolkit-agnostic tiling-layout resolver.
//!
//! Given a tab's server-authoritative [`LayoutNode`] tree and the available
//! content area (in cells), [`resolve_layout`] produces one [`PaneRect`] per
//! leaf, in cell coordinates. Every frontend (TUI, GTK, Swift via FFI) calls
//! this — none reimplements it — so all clients viewing the same tab compute
//! **identical** geometry for the same `(tree, area, config)`.
//!
//! Determinism is a hard requirement, not a nicety: each client resolves the
//! *same shared ratio tree* against *its own* window, then attaches each pane at
//! its computed cell size; the daemon's smallest-wins negotiation reconciles the
//! per-client sizes. If two clients with the same window disagreed by a cell,
//! the PTY would thrash. So the resolver uses **integer-only largest-remainder
//! apportionment** with stable tie-breaking, and subtracts gutters *before*
//! apportioning ratios.

use kmux_protocol::messages::{LayoutNode, SplitDir};

/// A resolved pane rectangle in cell coordinates within the tab content area.
/// `(col, row)` is the top-left corner; `cols`/`rows` are the extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub pane_index: u32,
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

/// Tunables for layout resolution.
#[derive(Debug, Clone, Copy)]
pub struct LayoutConfig {
    /// Divider width (cells) between horizontally-arranged children.
    pub gutter_cols: u16,
    /// Divider height (cells) between vertically-arranged children.
    pub gutter_rows: u16,
    /// Minimum width a pane is clamped to (never emit a 0-col PTY).
    pub min_cols: u16,
    /// Minimum height a pane is clamped to (never emit a 0-row PTY).
    pub min_rows: u16,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            gutter_cols: 1,
            gutter_rows: 1,
            min_cols: 1,
            min_rows: 1,
        }
    }
}

/// Direction for geometric focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Resolve a tab's layout tree into per-pane cell rectangles within an
/// `area_cols × area_rows` content area. The result is depth-first left-to-right
/// (so leaf order matches [`LayoutNode::leaves`]).
pub fn resolve_layout(
    node: &LayoutNode,
    area_cols: u16,
    area_rows: u16,
    cfg: &LayoutConfig,
) -> Vec<PaneRect> {
    let mut out = Vec::new();
    resolve_into(node, 0, 0, area_cols, area_rows, cfg, &mut out);
    out
}

fn resolve_into(
    node: &LayoutNode,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    cfg: &LayoutConfig,
    out: &mut Vec<PaneRect>,
) {
    match node {
        LayoutNode::Leaf { pane_index } => out.push(PaneRect {
            pane_index: *pane_index,
            col,
            row,
            cols: cols.max(cfg.min_cols),
            rows: rows.max(cfg.min_rows),
        }),
        LayoutNode::Split {
            dir,
            ratios,
            children,
        } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            match dir {
                SplitDir::Horizontal => {
                    // Children laid out left↔right; apportion the width axis.
                    let gutters = (n as u16 - 1).saturating_mul(cfg.gutter_cols);
                    let avail = cols.saturating_sub(gutters);
                    let widths = apportion(avail, ratios);
                    let mut x = col;
                    for (child, w) in children.iter().zip(widths) {
                        resolve_into(child, x, row, w, rows, cfg, out);
                        x = x.saturating_add(w).saturating_add(cfg.gutter_cols);
                    }
                }
                SplitDir::Vertical => {
                    // Children laid out top↕bottom; apportion the height axis.
                    let gutters = (n as u16 - 1).saturating_mul(cfg.gutter_rows);
                    let avail = rows.saturating_sub(gutters);
                    let heights = apportion(avail, ratios);
                    let mut y = row;
                    for (child, h) in children.iter().zip(heights) {
                        resolve_into(child, col, y, cols, h, cfg, out);
                        y = y.saturating_add(h).saturating_add(cfg.gutter_rows);
                    }
                }
            }
        }
    }
}

/// Apportion `total` integer cells among `ratios` (permille weights) using
/// largest-remainder, so the parts sum to exactly `total`. Deterministic:
/// remainder ties break toward the lower index.
fn apportion(total: u16, ratios: &[u16]) -> Vec<u16> {
    let n = ratios.len();
    if n == 0 {
        return Vec::new();
    }
    let sum: u32 = ratios.iter().map(|&r| r as u32).sum::<u32>().max(1);
    let total = total as u32;
    let mut alloc = vec![0u16; n];
    let mut remainders: Vec<(usize, u32)> = Vec::with_capacity(n);
    let mut used: u32 = 0;
    for (i, &r) in ratios.iter().enumerate() {
        let num = r as u32 * total;
        let q = num / sum;
        alloc[i] = q as u16;
        remainders.push((i, num % sum));
        used += q;
    }
    let mut leftover = total.saturating_sub(used);
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (i, _) in remainders {
        if leftover == 0 {
            break;
        }
        alloc[i] += 1;
        leftover -= 1;
    }
    alloc
}

/// Find the pane to focus when moving from `focused` in `dir`, using the
/// resolved geometry. Picks the nearest pane on the requested side, tie-broken
/// by greater overlap on the perpendicular axis (the tmux/zellij heuristic).
pub fn focus_neighbor(rects: &[PaneRect], focused: u32, dir: FocusDir) -> Option<u32> {
    let cur = rects.iter().find(|r| r.pane_index == focused)?;
    let mut best: Option<(&PaneRect, i32, u32)> = None; // (rect, primary distance, overlap)
    for r in rects {
        if r.pane_index == focused {
            continue;
        }
        let (on_side, primary_dist, overlap) = match dir {
            FocusDir::Left => (
                right_edge(r) <= cur.col,
                cur.col as i32 - right_edge(r) as i32,
                axis_overlap(cur.row, cur.rows, r.row, r.rows),
            ),
            FocusDir::Right => (
                r.col >= right_edge(cur),
                r.col as i32 - right_edge(cur) as i32,
                axis_overlap(cur.row, cur.rows, r.row, r.rows),
            ),
            FocusDir::Up => (
                bottom_edge(r) <= cur.row,
                cur.row as i32 - bottom_edge(r) as i32,
                axis_overlap(cur.col, cur.cols, r.col, r.cols),
            ),
            FocusDir::Down => (
                r.row >= bottom_edge(cur),
                r.row as i32 - bottom_edge(cur) as i32,
                axis_overlap(cur.col, cur.cols, r.col, r.cols),
            ),
        };
        if !on_side || overlap == 0 {
            continue;
        }
        let better = match best {
            None => true,
            // Closer wins; on a tie, more perpendicular overlap wins.
            Some((_, bd, bo)) => primary_dist < bd || (primary_dist == bd && overlap > bo),
        };
        if better {
            best = Some((r, primary_dist, overlap));
        }
    }
    best.map(|(r, _, _)| r.pane_index)
}

fn right_edge(r: &PaneRect) -> u16 {
    r.col.saturating_add(r.cols)
}

fn bottom_edge(r: &PaneRect) -> u16 {
    r.row.saturating_add(r.rows)
}

/// Overlap length of two 1-D intervals `[a0, a0+a_len)` and `[b0, b0+b_len)`.
fn axis_overlap(a0: u16, a_len: u16, b0: u16, b_len: u16) -> u32 {
    let a_end = a0 as u32 + a_len as u32;
    let b_end = b0 as u32 + b_len as u32;
    let lo = (a0 as u32).max(b0 as u32);
    let hi = a_end.min(b_end);
    hi.saturating_sub(lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_no_gutter() -> LayoutConfig {
        LayoutConfig {
            gutter_cols: 0,
            gutter_rows: 0,
            min_cols: 1,
            min_rows: 1,
        }
    }

    fn leaf(i: u32) -> LayoutNode {
        LayoutNode::Leaf { pane_index: i }
    }

    #[test]
    fn single_leaf_fills_area() {
        let rects = resolve_layout(&leaf(0), 80, 24, &cfg_no_gutter());
        assert_eq!(rects.len(), 1);
        assert_eq!(
            rects[0],
            PaneRect {
                pane_index: 0,
                col: 0,
                row: 0,
                cols: 80,
                rows: 24
            }
        );
    }

    #[test]
    fn horizontal_split_halves_width_and_tiles_exactly() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(0), leaf(1)],
        };
        let rects = resolve_layout(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].col, 0);
        assert_eq!(rects[0].cols, 40);
        assert_eq!(rects[1].col, 40);
        assert_eq!(rects[1].cols, 40);
        // Both full height; widths tile the area exactly (no gutter).
        assert_eq!(rects[0].cols + rects[1].cols, 80);
        assert!(rects.iter().all(|r| r.rows == 24));
    }

    #[test]
    fn apportion_is_exact_and_largest_remainder() {
        // 100 cells, thirds → 34/33/33 (leftover to lowest index).
        let a = apportion(100, &[333, 333, 333]);
        assert_eq!(a, vec![34, 33, 33]);
        assert_eq!(a.iter().map(|&x| x as u32).sum::<u32>(), 100);
    }

    #[test]
    fn gutters_are_subtracted_before_apportioning() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(0), leaf(1)],
        };
        // 1-col gutter: 81 - 1 = 80 split into 40/40, gutter at col 40.
        let rects = resolve_layout(&tree, 81, 24, &LayoutConfig::default());
        assert_eq!(rects[0].col, 0);
        assert_eq!(rects[0].cols, 40);
        assert_eq!(rects[1].col, 41); // 40 + 1 gutter
        assert_eq!(rects[1].cols, 40);
    }

    #[test]
    fn nested_split_resolves_recursively() {
        // Left half a single pane; right half split vertically into two.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![
                leaf(0),
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    ratios: vec![500, 500],
                    children: vec![leaf(1), leaf(2)],
                },
            ],
        };
        let rects = resolve_layout(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(rects.len(), 3);
        // Leaf order is depth-first left-to-right.
        assert_eq!(rects[0].pane_index, 0);
        assert_eq!(rects[1].pane_index, 1);
        assert_eq!(rects[2].pane_index, 2);
        // Right children share the right half's width, stacked vertically.
        assert_eq!(rects[1].col, 40);
        assert_eq!(rects[2].col, 40);
        assert_eq!(rects[1].row, 0);
        assert_eq!(rects[2].row, 12);
        assert_eq!(rects[1].rows + rects[2].rows, 24);
    }

    #[test]
    fn resolution_is_deterministic() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![300, 300, 400],
            children: vec![leaf(0), leaf(1), leaf(2)],
        };
        let a = resolve_layout(&tree, 97, 31, &LayoutConfig::default());
        let b = resolve_layout(&tree, 97, 31, &LayoutConfig::default());
        assert_eq!(a, b, "same input must yield identical rects");
    }

    #[test]
    fn focus_neighbor_horizontal() {
        // Two panes side by side: 0 | 1.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(0), leaf(1)],
        };
        let rects = resolve_layout(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(focus_neighbor(&rects, 0, FocusDir::Right), Some(1));
        assert_eq!(focus_neighbor(&rects, 1, FocusDir::Left), Some(0));
        // No neighbor off the edges.
        assert_eq!(focus_neighbor(&rects, 0, FocusDir::Left), None);
        assert_eq!(focus_neighbor(&rects, 1, FocusDir::Up), None);
    }

    #[test]
    fn focus_neighbor_prefers_overlap() {
        // Left tall pane 0; right side split into top (1) and bottom (2).
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![
                leaf(0),
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    ratios: vec![500, 500],
                    children: vec![leaf(1), leaf(2)],
                },
            ],
        };
        let rects = resolve_layout(&tree, 80, 24, &cfg_no_gutter());
        // Moving right from the tall left pane lands on the top-right pane
        // (first by distance tie, then it overlaps rows 0..12).
        let n = focus_neighbor(&rects, 0, FocusDir::Right);
        assert!(n == Some(1) || n == Some(2));
        // From the top-right pane, down goes to the bottom-right pane.
        assert_eq!(focus_neighbor(&rects, 1, FocusDir::Down), Some(2));
        assert_eq!(focus_neighbor(&rects, 2, FocusDir::Up), Some(1));
    }
}
