//! Pure, PTY-free mutations of a tab's [`LayoutNode`] tree.
//!
//! These functions are the server-authoritative source of layout truth: every
//! client layout intent (split, close, swap, resize, focus) is applied here
//! under the `sessions` write lock and the resulting tree broadcast via
//! `LayoutUpdate`. They operate purely on the tree (leaves reference panes by
//! `pane_index`), so they are unit-testable without spawning a PTY.
//!
//! Ratios are **permille** (`u16`, summing to 1000). All mutations renormalize
//! so the invariant holds, keeping the tree deterministic across clients.

use kmux_protocol::messages::{LayoutNode, SplitDir};

/// Lowest permille weight a child may hold, so a pane can never be fully
/// collapsed to zero by a resize. Cell-level minimums are enforced separately
/// by the shared resolver in `kmux-app`.
const MIN_RATIO: u16 = 20;

/// Rescale `ratios` in place so they sum to exactly 1000, using largest-remainder
/// apportionment (deterministic). An all-zero input becomes an equal split.
pub fn normalize(ratios: &mut [u16]) {
    if ratios.is_empty() {
        return;
    }
    let n = ratios.len();
    let sum: u32 = ratios.iter().map(|&r| r as u32).sum();
    if sum == 0 {
        let base = (1000 / n) as u16;
        for r in ratios.iter_mut() {
            *r = base;
        }
        let mut leftover = 1000 - base as u32 * n as u32;
        for r in ratios.iter_mut() {
            if leftover == 0 {
                break;
            }
            *r += 1;
            leftover -= 1;
        }
        return;
    }
    // Floor each to permille-of-1000; hand out the remaining units to the
    // largest fractional remainders.
    let mut remainders: Vec<(usize, u32)> = Vec::with_capacity(n);
    let mut total: u32 = 0;
    for (i, r) in ratios.iter_mut().enumerate() {
        let num = *r as u32 * 1000;
        let q = num / sum;
        remainders.push((i, num % sum));
        *r = q as u16;
        total += q;
    }
    let mut leftover = 1000u32.saturating_sub(total);
    // Stable: break remainder ties by lower index.
    remainders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (i, _) in remainders {
        if leftover == 0 {
            break;
        }
        ratios[i] += 1;
        leftover -= 1;
    }
}

/// Split the leaf `from_pane`, inserting `new_pane` adjacent to it in `dir`.
///
/// If `from_pane`'s parent already splits in the same `dir`, the new pane is
/// appended as a sibling (flatter, tmux-like) rather than nesting. Returns
/// `false` if `from_pane` is not a leaf in the tree.
pub fn split(root: &mut LayoutNode, from_pane: u32, new_pane: u32, dir: SplitDir) -> bool {
    // Root-is-the-target-leaf: wrap it in a fresh split.
    if let LayoutNode::Leaf { pane_index } = root {
        if *pane_index == from_pane {
            let old = std::mem::replace(
                root,
                LayoutNode::Leaf {
                    pane_index: from_pane,
                },
            );
            *root = LayoutNode::Split {
                dir,
                ratios: vec![500, 500],
                children: vec![
                    old,
                    LayoutNode::Leaf {
                        pane_index: new_pane,
                    },
                ],
            };
            return true;
        }
        return false;
    }
    split_in_split(root, from_pane, new_pane, dir)
}

fn split_in_split(node: &mut LayoutNode, from_pane: u32, new_pane: u32, dir: SplitDir) -> bool {
    let LayoutNode::Split {
        dir: my_dir,
        ratios,
        children,
    } = node
    else {
        return false;
    };
    let my_dir = *my_dir;
    if let Some(pos) = children.iter().position(is_leaf(from_pane)) {
        if my_dir == dir {
            // Append a sibling, halving the target's weight for the new pane.
            let share = ratios[pos];
            let new_share = share / 2;
            ratios[pos] = share - new_share;
            ratios.insert(pos + 1, new_share);
            children.insert(
                pos + 1,
                LayoutNode::Leaf {
                    pane_index: new_pane,
                },
            );
            normalize(ratios);
        } else {
            // Nest: replace the leaf with a split in the orthogonal direction.
            let old = std::mem::replace(
                &mut children[pos],
                LayoutNode::Leaf {
                    pane_index: from_pane,
                },
            );
            children[pos] = LayoutNode::Split {
                dir,
                ratios: vec![500, 500],
                children: vec![
                    old,
                    LayoutNode::Leaf {
                        pane_index: new_pane,
                    },
                ],
            };
        }
        return true;
    }
    for child in children.iter_mut() {
        if matches!(child, LayoutNode::Split { .. })
            && split_in_split(child, from_pane, new_pane, dir)
        {
            return true;
        }
    }
    false
}

/// Remove the leaf `pane` from the tree, collapsing the parent split when it
/// drops to a single child and redistributing weights. Returns `false` if
/// `pane` is the sole leaf (the caller should close the whole tab instead) or
/// is absent.
pub fn remove_pane(root: &mut LayoutNode, pane: u32) -> bool {
    match root {
        // Removing the only leaf is the caller's "close the tab" case.
        LayoutNode::Leaf { .. } => false,
        LayoutNode::Split { .. } => remove_in_split(root, pane),
    }
}

fn remove_in_split(node: &mut LayoutNode, pane: u32) -> bool {
    let LayoutNode::Split {
        ratios, children, ..
    } = node
    else {
        return false;
    };
    if let Some(pos) = children.iter().position(is_leaf(pane)) {
        children.remove(pos);
        ratios.remove(pos);
        if children.len() == 1 {
            *node = children.remove(0);
        } else {
            normalize(ratios);
        }
        return true;
    }
    for child in children.iter_mut() {
        if matches!(child, LayoutNode::Split { .. }) && remove_in_split(child, pane) {
            return true;
        }
    }
    false
}

/// Set the child weights of the `Split` addressed by `path` (child-index descent
/// from the root). Clamps each to a minimum, renormalizes to 1000, and returns
/// `false` if the path does not point at a `Split` or the arity mismatches.
pub fn set_ratios(root: &mut LayoutNode, path: &[u32], new_ratios: &[u16]) -> bool {
    let mut node = root;
    for &idx in path {
        let LayoutNode::Split { children, .. } = node else {
            return false;
        };
        let Some(child) = children.get_mut(idx as usize) else {
            return false;
        };
        node = child;
    }
    let LayoutNode::Split {
        ratios, children, ..
    } = node
    else {
        return false;
    };
    if new_ratios.len() != children.len() {
        return false;
    }
    *ratios = new_ratios.iter().map(|&r| r.max(MIN_RATIO)).collect();
    normalize(ratios);
    true
}

/// Swap the positions of two panes by exchanging their `pane_index` at the
/// leaves (split ratios untouched). Returns `false` unless both are leaves.
pub fn swap(root: &mut LayoutNode, a: u32, b: u32) -> bool {
    if a == b {
        return false;
    }
    let leaves = root.leaves();
    if !leaves.contains(&a) || !leaves.contains(&b) {
        return false;
    }
    rename_leaf(root, a, b);
    true
}

fn rename_leaf(node: &mut LayoutNode, a: u32, b: u32) {
    match node {
        LayoutNode::Leaf { pane_index } => {
            if *pane_index == a {
                *pane_index = b;
            } else if *pane_index == b {
                *pane_index = a;
            }
        }
        LayoutNode::Split { children, .. } => {
            for c in children.iter_mut() {
                rename_leaf(c, a, b);
            }
        }
    }
}

/// Choose which pane should receive focus after `removed` is closed, computed
/// from the **pre-removal** tree: prefer the previous sibling, else the next.
/// Returns `None` when `removed` is the root leaf (no sibling).
pub fn next_focus_after_removal(root: &LayoutNode, removed: u32) -> Option<u32> {
    if let LayoutNode::Split { children, .. } = root {
        if let Some(pos) = children.iter().position(is_leaf(removed)) {
            let sib = if pos > 0 {
                children.get(pos - 1)
            } else {
                children.get(pos + 1)
            };
            return sib.and_then(|s| s.leaves().first().copied());
        }
        for c in children {
            if let Some(f) = next_focus_after_removal(c, removed) {
                return Some(f);
            }
        }
    }
    None
}

/// Predicate: a `LayoutNode` that is exactly `Leaf { pane_index }`.
fn is_leaf(pane_index: u32) -> impl Fn(&LayoutNode) -> bool {
    move |n: &LayoutNode| matches!(n, LayoutNode::Leaf { pane_index: p } if *p == pane_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(i: u32) -> LayoutNode {
        LayoutNode::Leaf { pane_index: i }
    }

    fn sum(node: &LayoutNode) -> u32 {
        match node {
            LayoutNode::Leaf { .. } => 0,
            LayoutNode::Split {
                ratios, children, ..
            } => {
                let s: u32 = ratios.iter().map(|&r| r as u32).sum();
                s.max(children.iter().map(sum).max().unwrap_or(0))
            }
        }
    }

    /// Assert every `Split` in the tree has ratios summing to exactly 1000 and
    /// arity matching its child count.
    fn assert_well_formed(node: &LayoutNode) {
        if let LayoutNode::Split {
            ratios, children, ..
        } = node
        {
            assert_eq!(ratios.len(), children.len(), "arity mismatch");
            let s: u32 = ratios.iter().map(|&r| r as u32).sum();
            assert_eq!(s, 1000, "ratios must sum to 1000");
            for c in children {
                assert_well_formed(c);
            }
        }
    }

    #[test]
    fn normalize_sums_to_1000() {
        let mut r = vec![1, 1, 1];
        normalize(&mut r);
        assert_eq!(r.iter().map(|&x| x as u32).sum::<u32>(), 1000);
        // Largest-remainder gives the leftover unit to the first index on ties.
        assert_eq!(r, vec![334, 333, 333]);
    }

    #[test]
    fn normalize_all_zero_is_equal_split() {
        let mut r = vec![0, 0, 0, 0];
        normalize(&mut r);
        assert_eq!(r, vec![250, 250, 250, 250]);
    }

    #[test]
    fn split_root_leaf_creates_split() {
        let mut tree = leaf(0);
        assert!(split(&mut tree, 0, 1, SplitDir::Horizontal));
        assert_eq!(tree.leaves(), vec![0, 1]);
        assert_well_formed(&tree);
        assert_eq!(sum(&tree), 1000);
    }

    #[test]
    fn split_same_dir_appends_sibling_flat() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        // Splitting pane 1 horizontally again appends a third sibling (no nesting).
        split(&mut tree, 1, 2, SplitDir::Horizontal);
        match &tree {
            LayoutNode::Split { children, dir, .. } => {
                assert_eq!(*dir, SplitDir::Horizontal);
                assert_eq!(children.len(), 3, "should be a flat 3-way split");
            }
            _ => panic!("expected split"),
        }
        assert_eq!(tree.leaves(), vec![0, 1, 2]);
        assert_well_formed(&tree);
    }

    #[test]
    fn split_orthogonal_dir_nests() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        // Splitting pane 1 vertically nests it.
        split(&mut tree, 1, 2, SplitDir::Vertical);
        match &tree {
            LayoutNode::Split { children, .. } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(children[1], LayoutNode::Split { .. }));
            }
            _ => panic!("expected split"),
        }
        assert_eq!(tree.leaves(), vec![0, 1, 2]);
        assert_well_formed(&tree);
    }

    #[test]
    fn remove_collapses_parent_to_single_child() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        // Remove pane 1: the split collapses back to a bare leaf 0.
        assert!(remove_pane(&mut tree, 1));
        assert_eq!(tree, leaf(0));
    }

    #[test]
    fn remove_from_three_way_redistributes() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        split(&mut tree, 1, 2, SplitDir::Horizontal);
        assert!(remove_pane(&mut tree, 1));
        assert_eq!(tree.leaves(), vec![0, 2]);
        assert_well_formed(&tree);
    }

    #[test]
    fn remove_sole_leaf_returns_false() {
        let mut tree = leaf(0);
        assert!(!remove_pane(&mut tree, 0));
        assert_eq!(tree, leaf(0));
    }

    #[test]
    fn set_ratios_clamps_and_normalizes() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        // Path [] points at the root split.
        assert!(set_ratios(&mut tree, &[], &[900, 100]));
        match &tree {
            LayoutNode::Split { ratios, .. } => {
                assert_eq!(ratios.iter().map(|&x| x as u32).sum::<u32>(), 1000);
                assert_eq!(ratios[0], 900);
            }
            _ => panic!("expected split"),
        }
        // A zero is clamped to the minimum.
        assert!(set_ratios(&mut tree, &[], &[1000, 0]));
        match &tree {
            LayoutNode::Split { ratios, .. } => assert!(ratios[1] >= 1),
            _ => panic!(),
        }
    }

    #[test]
    fn set_ratios_rejects_bad_path_or_arity() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        // Path into a leaf is invalid.
        assert!(!set_ratios(&mut tree, &[0], &[500, 500]));
        // Wrong arity.
        assert!(!set_ratios(&mut tree, &[], &[1000]));
    }

    #[test]
    fn swap_exchanges_leaves() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        split(&mut tree, 1, 2, SplitDir::Vertical);
        assert!(swap(&mut tree, 0, 2));
        assert_eq!(tree.leaves(), vec![2, 1, 0]);
        // Swapping a non-existent pane fails.
        assert!(!swap(&mut tree, 0, 99));
    }

    #[test]
    fn focus_after_removal_prefers_previous_sibling() {
        let mut tree = leaf(0);
        split(&mut tree, 0, 1, SplitDir::Horizontal);
        split(&mut tree, 1, 2, SplitDir::Horizontal);
        // Removing the middle pane focuses the previous sibling (0).
        assert_eq!(next_focus_after_removal(&tree, 1), Some(0));
        // Removing the first pane focuses the next sibling (1).
        assert_eq!(next_focus_after_removal(&tree, 0), Some(1));
        // Sole-leaf tree has no sibling.
        assert_eq!(next_focus_after_removal(&leaf(5), 5), None);
    }
}
