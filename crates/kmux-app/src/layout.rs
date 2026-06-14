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
        } => match dir {
            SplitDir::Horizontal => {
                // Children laid out left↔right; apportion the width axis.
                for ((x, w), child) in child_extents(col, cols, ratios, cfg.gutter_cols)
                    .into_iter()
                    .zip(children)
                {
                    resolve_into(child, x, row, w, rows, cfg, out);
                }
            }
            SplitDir::Vertical => {
                // Children laid out top↕bottom; apportion the height axis.
                for ((y, h), child) in child_extents(row, rows, ratios, cfg.gutter_rows)
                    .into_iter()
                    .zip(children)
                {
                    resolve_into(child, col, y, cols, h, cfg, out);
                }
            }
        },
    }
}

/// The `(start, len)` of each child of a split along its layout axis: gutters
/// subtracted before apportioning, children placed with one gutter between
/// them. Shared by [`resolve_into`] (pane geometry) and [`resolve_dividers`]
/// (boundary hit regions) so the two can never disagree about where a child —
/// or the gutter after it — begins. `axis_start`/`axis_total` are the split's
/// own offset and extent on the relevant axis (col/cols for `Horizontal`,
/// row/rows for `Vertical`).
fn child_extents(axis_start: u16, axis_total: u16, ratios: &[u16], gutter: u16) -> Vec<(u16, u16)> {
    let n = ratios.len();
    if n == 0 {
        return Vec::new();
    }
    let gutters = (n as u16 - 1).saturating_mul(gutter);
    let avail = axis_total.saturating_sub(gutters);
    let lens = apportion(avail, ratios);
    let mut out = Vec::with_capacity(n);
    let mut pos = axis_start;
    for len in lens {
        out.push((pos, len));
        pos = pos.saturating_add(len).saturating_add(gutter);
    }
    out
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

// ── Draggable dividers ───────────────────────────────────────────────────────

/// A draggable boundary between two adjacent children of a [`LayoutNode::Split`],
/// in cell coordinates within the resolved content area. Frontends hit-test a
/// pointer against `hit_*` (the gutter strip) and feed a drag position to
/// [`ratios_for_drag`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divider {
    /// Child-index descent from the layout root to the `Split` this boundary
    /// belongs to — the same `path` form [`resize_split`] returns and the
    /// server's `set_ratios` expects.
    pub path: Vec<u32>,
    /// Orientation of the owning split. `Horizontal` → a *vertical* bar dragged
    /// along the col axis (a "col-resize"); `Vertical` → a *horizontal* bar
    /// dragged along the row axis (a "row-resize").
    pub dir: SplitDir,
    /// The boundary sits between `children[before]` and `children[before + 1]`.
    pub before: usize,
    /// Gutter strip rectangle in cells (the divider itself), for hit-testing.
    pub hit_col: u16,
    pub hit_row: u16,
    pub hit_cols: u16,
    pub hit_rows: u16,
    /// Start cell of `children[before]` on the drag axis (col for `Horizontal`,
    /// row for `Vertical`) — the origin the pointer fraction is measured from.
    pub pair_start: u16,
    /// Combined extent of the two adjacent children on the drag axis, *excluding*
    /// the gutter between them. The drag fraction is
    /// `(pointer - pair_start) / pair_len`.
    pub pair_len: u16,
}

/// Enumerate every draggable divider for a layout tree resolved into an
/// `area_cols × area_rows` content area. A `Split` with `n` children yields
/// `n - 1` dividers; a single `Leaf` yields none — so a zoomed
/// `render_layout()` (a single leaf) has zero dividers and nothing to drag.
/// Order is depth-first, matching [`resolve_layout`]; divider hit cells fall in
/// the exact gutters `resolve_layout` leaves blank (both use [`child_extents`]).
pub fn resolve_dividers(
    node: &LayoutNode,
    area_cols: u16,
    area_rows: u16,
    cfg: &LayoutConfig,
) -> Vec<Divider> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    dividers_into(node, 0, 0, area_cols, area_rows, cfg, &mut path, &mut out);
    out
}

#[allow(clippy::too_many_arguments)]
fn dividers_into(
    node: &LayoutNode,
    col: u16,
    row: u16,
    cols: u16,
    rows: u16,
    cfg: &LayoutConfig,
    path: &mut Vec<u32>,
    out: &mut Vec<Divider>,
) {
    let LayoutNode::Split {
        dir,
        ratios,
        children,
    } = node
    else {
        return;
    };
    let n = children.len();
    if n == 0 {
        return;
    }
    let (extents, gutter) = match dir {
        SplitDir::Horizontal => (
            child_extents(col, cols, ratios, cfg.gutter_cols),
            cfg.gutter_cols,
        ),
        SplitDir::Vertical => (
            child_extents(row, rows, ratios, cfg.gutter_rows),
            cfg.gutter_rows,
        ),
    };
    // A divider per boundary between consecutive children: the gutter strip
    // after `children[before]`.
    for before in 0..n - 1 {
        let (s0, l0) = extents[before];
        let (_, l1) = extents[before + 1];
        let pair_len = l0.saturating_add(l1);
        out.push(match dir {
            SplitDir::Horizontal => Divider {
                path: path.clone(),
                dir: *dir,
                before,
                hit_col: s0.saturating_add(l0),
                hit_row: row,
                hit_cols: gutter,
                hit_rows: rows,
                pair_start: s0,
                pair_len,
            },
            SplitDir::Vertical => Divider {
                path: path.clone(),
                dir: *dir,
                before,
                hit_col: col,
                hit_row: s0.saturating_add(l0),
                hit_cols: cols,
                hit_rows: gutter,
                pair_start: s0,
                pair_len,
            },
        });
    }
    // Recurse into each child's sub-rect, carrying the descent path.
    for (i, child) in children.iter().enumerate() {
        let (start, len) = extents[i];
        path.push(i as u32);
        match dir {
            SplitDir::Horizontal => dividers_into(child, start, row, len, rows, cfg, path, out),
            SplitDir::Vertical => dividers_into(child, col, start, cols, len, cfg, path, out),
        }
        path.pop();
    }
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

/// Compute the new full ratios vector for the split a [`Divider`] belongs to,
/// when its boundary is dragged so the divider sits at cell `pointer_cell` along
/// the drag axis. Child `before`'s share of the pair's *combined* permille
/// becomes its fraction of `pair_len`; the remainder goes to `before + 1`. Both
/// are clamped to `MIN_RESIZE_RATIO` (the same floor keyboard resize uses, and
/// the server's `MIN_RATIO`), so neither collapses; the other children's ratios
/// are untouched, so the split still sums to its original total.
///
/// Returns the full ratios vec (arity `== children.len()`, the form `set_ratios`
/// expects), or `None` when the split is missing/reshaped, the pair can't satisfy
/// both floors, or the result equals the current ratios (a no-op — e.g. clamped
/// at the floor, or the divider didn't move a whole cell).
pub fn ratios_for_drag(root: &LayoutNode, div: &Divider, pointer_cell: u16) -> Option<Vec<u16>> {
    let LayoutNode::Split {
        dir,
        ratios,
        children,
    } = node_at(root, &div.path)?
    else {
        return None;
    };
    // Geometry came from this same tree; bail if it has since been reshaped.
    if *dir != div.dir {
        return None;
    }
    let a = div.before;
    let b = a + 1;
    if b >= children.len() {
        return None;
    }
    let pair = ratios[a] as i32 + ratios[b] as i32;
    if pair < MIN_RESIZE_RATIO * 2 {
        return None; // Can't keep both children at/above the floor.
    }
    let len = div.pair_len.max(1) as i32;
    let off = (pointer_cell as i32 - div.pair_start as i32).clamp(0, len);
    // Child `a`'s new permille share of the pair, by axis fraction, rounded to
    // the nearest, then clamped so both children stay above the floor.
    let new_a = ((off * pair + len / 2) / len).clamp(MIN_RESIZE_RATIO, pair - MIN_RESIZE_RATIO);
    let mut out = ratios.clone();
    out[a] = new_a as u16;
    out[b] = (pair - new_a) as u16;
    if out == *ratios {
        return None; // No change (already there, or sub-cell movement).
    }
    Some(out)
}

/// Even permille weights for an `n`-child split, for resetting a split to equal
/// sizes (e.g. double-clicking a divider). The server clamps + renormalizes to
/// exactly 1000, so a remainder from the integer division is harmless. Empty for
/// `n == 0`.
pub fn even_ratios(n: usize) -> Vec<u16> {
    if n == 0 {
        return Vec::new();
    }
    vec![(1000 / n as u16).max(1); n]
}

/// Even ratios sized to the split addressed by `path` (for resetting that split
/// to equal children — e.g. double-clicking a divider). `None` when `path` does
/// not address a `Split`. The arity matches what `set_ratios` expects.
pub fn even_ratios_at(root: &LayoutNode, path: &[u32]) -> Option<Vec<u16>> {
    match node_at(root, path)? {
        LayoutNode::Split { children, .. } => Some(even_ratios(children.len())),
        _ => None,
    }
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

    // ── Dividers ─────────────────────────────────────────────────────────────

    fn vsplit(a: u32, b: u32) -> LayoutNode {
        LayoutNode::Split {
            dir: SplitDir::Vertical,
            ratios: vec![500, 500],
            children: vec![leaf(a), leaf(b)],
        }
    }

    #[test]
    fn dividers_single_leaf_is_empty() {
        // A single leaf (also what a zoomed `render_layout()` collapses to) has
        // no draggable boundary.
        assert!(resolve_dividers(&leaf(0), 80, 24, &LayoutConfig::default()).is_empty());
    }

    #[test]
    fn dividers_two_pane_horizontal() {
        // 81 cols, 1-col gutter → 40 | gutter@40 | 40.
        let divs = resolve_dividers(&hsplit(0, 1), 81, 24, &LayoutConfig::default());
        assert_eq!(divs.len(), 1);
        let d = &divs[0];
        assert_eq!(d.path, Vec::<u32>::new());
        assert_eq!(d.dir, SplitDir::Horizontal);
        assert_eq!(d.before, 0);
        assert_eq!(
            (d.hit_col, d.hit_row, d.hit_cols, d.hit_rows),
            (40, 0, 1, 24)
        );
        assert_eq!((d.pair_start, d.pair_len), (0, 80));
    }

    #[test]
    fn dividers_three_way_has_two() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![300, 300, 400],
            children: vec![leaf(0), leaf(1), leaf(2)],
        };
        let divs = resolve_dividers(&tree, 100, 24, &cfg_no_gutter());
        assert_eq!(divs.len(), 2);
        assert_eq!(divs[0].before, 0);
        assert_eq!(divs[1].before, 1);
        assert!(divs.iter().all(|d| d.path.is_empty()));
    }

    #[test]
    fn dividers_nested_both_axes() {
        // Left leaf 0; right half a vertical split of 1 (top) / 2 (bottom).
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![leaf(0), vsplit(1, 2)],
        };
        let divs = resolve_dividers(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(divs.len(), 2);
        // Root horizontal divider between the left pane and the right column.
        assert_eq!(divs[0].path, Vec::<u32>::new());
        assert_eq!(divs[0].dir, SplitDir::Horizontal);
        // Inner vertical divider inside the right column (path descends child 1).
        assert_eq!(divs[1].path, vec![1]);
        assert_eq!(divs[1].dir, SplitDir::Vertical);
        // The inner divider lives in the right half, stacked at mid-height.
        assert_eq!(divs[1].hit_col, 40);
        assert_eq!(divs[1].hit_row, 12);
    }

    #[test]
    fn dividers_land_in_resolver_gutters() {
        // Each flat-split divider sits in the gutter right after the matching
        // pane rect — proving `resolve_dividers` and `resolve_layout` share
        // `child_extents` and never drift.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![300, 300, 400],
            children: vec![leaf(0), leaf(1), leaf(2)],
        };
        let rects = resolve_layout(&tree, 100, 24, &LayoutConfig::default());
        let divs = resolve_dividers(&tree, 100, 24, &LayoutConfig::default());
        for d in &divs {
            let r = &rects[d.before];
            assert_eq!(d.hit_col, r.col + r.cols, "divider after pane {}", d.before);
            assert_eq!(d.hit_cols, 1); // default gutter
            assert_eq!((d.hit_row, d.hit_rows), (0, 24));
        }
    }

    #[test]
    fn drag_sets_ratios_by_fraction() {
        // Drag the 2-pane divider to 25% of the 80-cell pair span → ~[250, 750].
        let tree = hsplit(0, 1);
        let divs = resolve_dividers(&tree, 80, 24, &cfg_no_gutter());
        let ratios = ratios_for_drag(&tree, &divs[0], 20).unwrap();
        assert_eq!(ratios, vec![250, 750]);
        assert_eq!(ratios.iter().map(|&x| x as u32).sum::<u32>(), 1000);
    }

    #[test]
    fn drag_clamps_to_min_and_then_no_ops() {
        let tree = hsplit(0, 1);
        let divs = resolve_dividers(&tree, 80, 24, &cfg_no_gutter());
        // Dragging to the far edge clamps child 0 to the floor (20).
        let clamped = ratios_for_drag(&tree, &divs[0], 0).unwrap();
        assert_eq!(clamped, vec![20, 980]);
        // From a tree already at the floor, the same drag is a no-op.
        let floored = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![20, 980],
            children: vec![leaf(0), leaf(1)],
        };
        let fdivs = resolve_dividers(&floored, 80, 24, &cfg_no_gutter());
        assert_eq!(ratios_for_drag(&floored, &fdivs[0], 0), None);
    }

    #[test]
    fn drag_is_deterministic() {
        let tree = hsplit(0, 1);
        let divs = resolve_dividers(&tree, 97, 31, &LayoutConfig::default());
        let a = ratios_for_drag(&tree, &divs[0], 30);
        let b = ratios_for_drag(&tree, &divs[0], 30);
        assert_eq!(a, b);
    }

    #[test]
    fn drag_only_touches_the_pair_in_a_three_way() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![300, 300, 400],
            children: vec![leaf(0), leaf(1), leaf(2)],
        };
        let divs = resolve_dividers(&tree, 100, 24, &cfg_no_gutter());
        // Drag the first divider (between panes 0 and 1); pane 2 is untouched.
        let ratios = ratios_for_drag(&tree, &divs[0], 15).unwrap();
        assert_eq!(ratios, vec![150, 450, 400]);
    }

    #[test]
    fn drag_vertical_axis() {
        // A vertical split's divider moves on the row axis.
        let tree = vsplit(0, 1);
        let divs = resolve_dividers(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(divs[0].dir, SplitDir::Vertical);
        let ratios = ratios_for_drag(&tree, &divs[0], 6).unwrap();
        assert_eq!(ratios, vec![250, 750]);
    }

    #[test]
    fn even_ratios_are_balanced() {
        assert_eq!(even_ratios(2), vec![500, 500]);
        assert_eq!(even_ratios(3), vec![333, 333, 333]);
        assert!(even_ratios(0).is_empty());
    }

    // ── Resize/focus arithmetic edge cases ──────────────────────────────────
    // The tests above pin typical 2-pane resizes and basic focus adjacency.
    // These probe the boundary inputs the arithmetic actually turns on:
    // oversized deltas, the MIN_RESIZE_RATIO floor, splits wider than two
    // children, and genuine focus distance ties.

    #[test]
    fn shift_pair_preserves_sum_and_floors_the_shrinking_side() {
        // Grow `a` by far more than the pair can give: `b` must stop at the
        // floor (never wrap below zero) and the pairwise sum stays exact.
        let mut r = [500u16, 500];
        shift_pair(&mut r, 0, 1, 600);
        assert_eq!(r[0] as i32 + r[1] as i32, 1000, "pairwise sum is invariant");
        assert_eq!(
            r[1],
            MIN_RESIZE_RATIO as u16,
            "the shrinking side clamps to the floor instead of underflowing"
        );
        assert_eq!(r[0], 1000 - MIN_RESIZE_RATIO as u16);

        // Symmetric: a large negative delta floors `a`.
        let mut r = [500u16, 500];
        shift_pair(&mut r, 0, 1, -600);
        assert_eq!(r[0], MIN_RESIZE_RATIO as u16);
        assert_eq!(r[1], 1000 - MIN_RESIZE_RATIO as u16);
    }

    #[test]
    fn shift_pair_is_a_noop_when_the_pair_cannot_hold_two_minimums() {
        // total < 2*MIN: there is no rebalance that keeps both children at the
        // floor, so the pair is left untouched rather than forced invalid.
        let mut r = [MIN_RESIZE_RATIO as u16 - 1, MIN_RESIZE_RATIO as u16 - 1];
        let before = r;
        shift_pair(&mut r, 0, 1, 5);
        assert_eq!(r, before, "a pair below 2*MIN is never modified");
    }

    #[test]
    fn resize_split_handles_a_flat_three_way_split() {
        // The 2-pane tests never exercise n > 2. Growing the middle pane trades
        // only with its next sibling, leaving the first pane untouched.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![300, 300, 400],
            children: vec![leaf(0), leaf(1), leaf(2)],
        };
        let (path, ratios) = resize_split(&tree, 1, FocusDir::Right, 50).unwrap();
        assert_eq!(path, Vec::<u32>::new());
        assert_eq!(ratios, vec![300, 350, 350], "pane 1 grows into pane 2 only");
        assert_eq!(ratios.iter().map(|&x| x as u32).sum::<u32>(), 1000);
    }

    #[test]
    fn focus_neighbor_breaks_distance_ties_by_perpendicular_overlap() {
        // Left pane spans the full height; the right column is split unevenly
        // into a tall top pane (1) and a short bottom pane (2). Both right panes
        // sit the same distance to the right of pane 0, so the tie is broken by
        // overlap — pane 1 shares more rows with pane 0 and must win.
        let tree = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratios: vec![500, 500],
            children: vec![
                leaf(0),
                LayoutNode::Split {
                    dir: SplitDir::Vertical,
                    ratios: vec![750, 250],
                    children: vec![leaf(1), leaf(2)],
                },
            ],
        };
        let rects = resolve_layout(&tree, 80, 24, &cfg_no_gutter());
        assert_eq!(
            focus_neighbor(&rects, 0, FocusDir::Right),
            Some(1),
            "a distance tie breaks toward the neighbor with greater overlap"
        );
    }
}
