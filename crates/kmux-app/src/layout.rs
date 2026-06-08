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

// ── Keyboard resize ──────────────────────────────────────────────────────────

/// Permille a single keyboard resize step shifts a split boundary (5%).
pub const RESIZE_STEP_PERMILLE: u16 = 50;

/// Lowest permille weight a resize leaves a child with, mirroring the server's
/// `MIN_RATIO` so client and server agree on the clamp.
const MIN_RESIZE_RATIO: i32 = 20;

/// Compute the `SetLayoutRatios` payload to resize the `focused` pane in `dir`
/// by `step` permille.
///
/// Finds the nearest ancestor [`LayoutNode::Split`] of the focused pane on the
/// matching axis — `Horizontal` for `Left`/`Right`, `Vertical` for `Up`/`Down` —
/// and shifts `step` permille across the boundary between the focused subtree and
/// an adjacent sibling. `Right`/`Down` grow the focused pane; `Left`/`Up` shrink
/// it, regardless of which boundary moves, so the four arrows map cleanly onto
/// "wider/narrower/taller/shorter".
///
/// Returns `(path, new_ratios)` addressing that split (the `path` is a root
/// child-index descent, the form [`crate::layout`]'s server counterpart
/// `set_ratios` expects), or `None` when the focused pane has no resizable
/// ancestor on that axis or the move clamps to a no-op.
pub fn resize_split(
    root: &LayoutNode,
    focused: u32,
    dir: FocusDir,
    step: u16,
) -> Option<(Vec<u32>, Vec<u16>)> {
    let axis = match dir {
        FocusDir::Left | FocusDir::Right => SplitDir::Horizontal,
        FocusDir::Up | FocusDir::Down => SplitDir::Vertical,
    };
    let grow = matches!(dir, FocusDir::Right | FocusDir::Down);
    let (path, child) = nearest_split_on_axis(root, focused, axis)?;
    let LayoutNode::Split {
        ratios, children, ..
    } = node_at(root, &path)?
    else {
        return None;
    };
    let n = children.len();
    if n < 2 {
        return None;
    }
    // Trade `step` with the next sibling, or the previous one when the focused
    // child is last. Either way the focused child gains (grow) or loses (shrink),
    // so Left/Right purely change width and Up/Down purely change height.
    let neighbor = if child + 1 < n { child + 1 } else { child - 1 };
    let mut new = ratios.clone();
    let delta = if grow { step as i32 } else { -(step as i32) };
    shift_pair(&mut new, child, neighbor, delta);
    if new == *ratios {
        return None; // Clamped to a no-op (already at the minimum boundary).
    }
    Some((path, new))
}

/// Move `delta` permille into child `a` from child `b` (negative `delta` moves
/// the other way), preserving their pairwise sum and clamping both to a minimum
/// so neither collapses. Because only this pair changes, the split still sums to
/// the same total (1000).
fn shift_pair(ratios: &mut [u16], a: usize, b: usize, delta: i32) {
    let total = ratios[a] as i32 + ratios[b] as i32;
    if total < MIN_RESIZE_RATIO * 2 {
        return;
    }
    let na = (ratios[a] as i32 + delta).clamp(MIN_RESIZE_RATIO, total - MIN_RESIZE_RATIO);
    ratios[a] = na as u16;
    ratios[b] = (total - na) as u16;
}

/// Path (root child-index descent) to the nearest ancestor `Split` of `focused`
/// whose `dir == axis`, plus the index — within that split — of the child whose
/// subtree contains `focused`. `None` if no such ancestor exists.
fn nearest_split_on_axis(
    root: &LayoutNode,
    focused: u32,
    axis: SplitDir,
) -> Option<(Vec<u32>, usize)> {
    let mut descent: Vec<(SplitDir, u32)> = Vec::new();
    if !build_descent(root, focused, &mut descent) {
        return None;
    }
    // The deepest split on the axis is the innermost (nearest) ancestor.
    let k = descent.iter().rposition(|(d, _)| *d == axis)?;
    let path = descent[..k].iter().map(|(_, c)| *c).collect();
    Some((path, descent[k].1 as usize))
}

/// Record, for each `Split` on the path from `root` down to the leaf `focused`,
/// its direction and the child index taken. Returns whether `focused` was found
/// (leaving `acc` as the descent on success).
fn build_descent(node: &LayoutNode, focused: u32, acc: &mut Vec<(SplitDir, u32)>) -> bool {
    match node {
        LayoutNode::Leaf { pane_index } => *pane_index == focused,
        LayoutNode::Split { dir, children, .. } => {
            for (i, child) in children.iter().enumerate() {
                acc.push((*dir, i as u32));
                if build_descent(child, focused, acc) {
                    return true;
                }
                acc.pop();
            }
            false
        }
    }
}

/// The node reached by descending `path` (child indices) from `root`.
fn node_at<'a>(root: &'a LayoutNode, path: &[u32]) -> Option<&'a LayoutNode> {
    let mut node = root;
    for &idx in path {
        let LayoutNode::Split { children, .. } = node else {
            return None;
        };
        node = children.get(idx as usize)?;
    }
    Some(node)
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

    fn hsplit(a: u32, b: u32) -> LayoutNode {
        LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(a), leaf(b)],
        }
    }

    #[test]
    fn resize_grows_focused_toward_next_sibling() {
        // [0 | 1], focus 0, grow right: 0 takes from 1.
        let tree = hsplit(0, 1);
        let (path, ratios) = resize_split(&tree, 0, FocusDir::Right, 50).unwrap();
        assert_eq!(path, Vec::<u32>::new(), "the root split is addressed by []");
        assert_eq!(ratios, vec![550, 450]);
        // The pairwise total is preserved, so the split still sums to 1000.
        assert_eq!(ratios.iter().map(|&x| x as u32).sum::<u32>(), 1000);
    }

    #[test]
    fn resize_shrinks_focused_toward_next_sibling() {
        let tree = hsplit(0, 1);
        let (_, ratios) = resize_split(&tree, 0, FocusDir::Left, 50).unwrap();
        assert_eq!(ratios, vec![450, 550]);
    }

    #[test]
    fn resize_last_child_trades_with_previous() {
        // Focus the rightmost pane: growing right has no right sibling, so it
        // steals from the left sibling (still gets wider).
        let tree = hsplit(0, 1);
        let (_, ratios) = resize_split(&tree, 1, FocusDir::Right, 50).unwrap();
        assert_eq!(ratios, vec![450, 550]);
        let (_, ratios) = resize_split(&tree, 1, FocusDir::Left, 50).unwrap();
        assert_eq!(ratios, vec![550, 450]);
    }

    #[test]
    fn resize_wrong_axis_returns_none() {
        // A purely horizontal tree has no vertical split to resize.
        let tree = hsplit(0, 1);
        assert_eq!(resize_split(&tree, 0, FocusDir::Up, 50), None);
        assert_eq!(resize_split(&tree, 0, FocusDir::Down, 50), None);
    }

    #[test]
    fn resize_picks_nearest_ancestor_on_each_axis() {
        // Left leaf 0; right half is a vertical split of 1 (top) / 2 (bottom).
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(0), {
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    ratios: vec![500, 500],
                    children: vec![leaf(1), leaf(2)],
                }
            }],
        };
        // Horizontal resize of pane 1 acts on the root split (child 1 = the
        // right subtree), growing the whole right column.
        let (path, ratios) = resize_split(&tree, 1, FocusDir::Right, 50).unwrap();
        assert_eq!(path, Vec::<u32>::new());
        assert_eq!(ratios, vec![450, 550]);
        // Vertical resize of pane 1 acts on the inner split at path [1].
        let (path, ratios) = resize_split(&tree, 1, FocusDir::Down, 50).unwrap();
        assert_eq!(path, vec![1]);
        assert_eq!(ratios, vec![550, 450]);
    }

    #[test]
    fn resize_clamps_to_minimum_and_reports_no_op() {
        // Both children already at the floor: a further shrink is a no-op.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![20, 980],
            children: vec![leaf(0), leaf(1)],
        };
        // Shrinking pane 0 further can't drop it below the minimum → None.
        assert_eq!(resize_split(&tree, 0, FocusDir::Left, 50), None);
        // Growing it is still fine.
        let (_, ratios) = resize_split(&tree, 0, FocusDir::Right, 50).unwrap();
        assert_eq!(ratios, vec![70, 930]);
    }

    #[test]
    fn resize_root_leaf_returns_none() {
        assert_eq!(resize_split(&leaf(0), 0, FocusDir::Right, 50), None);
    }
}
