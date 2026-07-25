//! Framework-neutral tmux-style split-pane layout tree.
//!
//! Extracted from jterm3. The tree, ratio math, and pixel-geometry helpers are
//! pure data: rendering, hit-testing, and persistence stay in each app. The
//! divider thickness is passed into the geometry functions so apps with
//! different chrome share the same subdivision math.

/// Minimum share of the split axis a single pane may occupy.
pub const PANE_RATIO_MIN: f32 = 0.1;
/// Dragging a divider within this distance of the even point between its two
/// neighbor panes snaps them to equal size, so tidy layouts are easy to hit.
pub const SPLIT_SNAP_EPSILON: f32 = 0.02;

/// An axis-aligned pixel rectangle. Field names match the common UI-toolkit
/// shape (`x`/`y`/`width`/`height`) so app-side conversions stay mechanical.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The axis a split node divides its children along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children side by side (left | right), laid out in a row, split along x.
    Vertical,
    /// Children stacked (top / bottom), laid out in a column, split along y.
    Horizontal,
}

/// tmux-style recursive pane layout. A `Leaf` shows one session; a `Split`
/// divides the area among two or more children along one [`Axis`], each taking
/// its aligned share of `ratios` (which sums to ~1). Nesting a `Split` inside a
/// `Split` of the other axis is how arbitrary tiled layouts are built.
#[derive(Debug, Clone, PartialEq)]
pub enum PaneTree {
    /// A single pane holding the session at this index.
    Leaf(usize),
    /// Two or more children arranged along `axis`.
    Split {
        axis: Axis,
        children: Vec<PaneTree>,
        ratios: Vec<f32>,
    },
}

/// Identifies one divider: the child-index `path` from the root to the owning
/// `Split` node, plus which `gap` (between children `gap` and `gap + 1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DividerId {
    pub path: Vec<usize>,
    pub gap: usize,
}

impl PaneTree {
    pub fn is_leaf(&self) -> bool {
        matches!(self, PaneTree::Leaf(_))
    }

    /// Collect leaf session indices in depth-first (render) order.
    fn collect_leaves(&self, out: &mut Vec<usize>) {
        match self {
            PaneTree::Leaf(s) => out.push(*s),
            PaneTree::Split { children, .. } => {
                for c in children {
                    c.collect_leaves(out);
                }
            }
        }
    }

    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    pub fn leaf_count(&self) -> usize {
        match self {
            PaneTree::Leaf(_) => 1,
            PaneTree::Split { children, .. } => children.iter().map(PaneTree::leaf_count).sum(),
        }
    }

    pub fn contains_session(&self, session: usize) -> bool {
        match self {
            PaneTree::Leaf(s) => *s == session,
            PaneTree::Split { children, .. } => {
                children.iter().any(|c| c.contains_session(session))
            }
        }
    }

    /// Rewrite every leaf's session index (used after the sessions Vec shifts).
    pub fn remap_sessions(&mut self, f: &dyn Fn(usize) -> usize) {
        match self {
            PaneTree::Leaf(s) => *s = f(*s),
            PaneTree::Split { children, .. } => {
                for c in children.iter_mut() {
                    c.remap_sessions(f);
                }
            }
        }
    }

    /// Child-index path from the root to the leaf showing `session`.
    pub fn path_to_session(&self, session: usize) -> Option<Vec<usize>> {
        fn walk(node: &PaneTree, session: usize, acc: &mut Vec<usize>) -> bool {
            match node {
                PaneTree::Leaf(s) => *s == session,
                PaneTree::Split { children, .. } => {
                    for (i, c) in children.iter().enumerate() {
                        acc.push(i);
                        if walk(c, session, acc) {
                            return true;
                        }
                        acc.pop();
                    }
                    false
                }
            }
        }
        let mut acc = Vec::new();
        walk(self, session, &mut acc).then_some(acc)
    }

    /// Prepare `target` to receive focus without changing the pane topology.
    ///
    /// If the target is already visible, the tree is left untouched so focusing
    /// another pane can never duplicate that session. Otherwise the focused leaf
    /// is replaced in place, preserving every split axis and ratio. Returns
    /// `false` only when `focused` is not a leaf in this tree.
    pub fn focus_or_replace_session(&mut self, focused: usize, target: usize) -> bool {
        if self.contains_session(target) {
            return true;
        }
        let Some(path) = self.path_to_session(focused) else {
            return false;
        };
        let Some(PaneTree::Leaf(session)) = self.node_at_path_mut(&path) else {
            return false;
        };
        *session = target;
        true
    }

    pub fn node_at_path_mut(&mut self, path: &[usize]) -> Option<&mut PaneTree> {
        let mut node = self;
        for &i in path {
            match node {
                PaneTree::Split { children, .. } => node = children.get_mut(i)?,
                PaneTree::Leaf(_) => return None,
            }
        }
        Some(node)
    }

    /// Split the leaf showing `target` along `axis`, inserting a new leaf for
    /// `new`. Matches tmux: if the leaf's parent already splits along `axis` the
    /// new pane joins as a sibling; otherwise the leaf is replaced by a fresh
    /// two-child split of the requested axis. Returns whether the target existed.
    pub fn split_leaf(&mut self, target: usize, axis: Axis, new: usize) -> bool {
        match self {
            PaneTree::Leaf(s) => {
                if *s == target {
                    *self = PaneTree::Split {
                        axis,
                        children: vec![PaneTree::Leaf(target), PaneTree::Leaf(new)],
                        ratios: vec![0.5, 0.5],
                    };
                    true
                } else {
                    false
                }
            }
            PaneTree::Split {
                axis: node_axis,
                children,
                ratios,
            } => {
                if *node_axis == axis {
                    if let Some(i) = children
                        .iter()
                        .position(|c| matches!(c, PaneTree::Leaf(s) if *s == target))
                    {
                        children.insert(i + 1, PaneTree::Leaf(new));
                        insert_pane_share(ratios, i);
                        return true;
                    }
                }
                children.iter_mut().any(|c| c.split_leaf(target, axis, new))
            }
        }
    }

    /// Remove the leaf showing `target`, folding its share into a sibling and
    /// collapsing any split left with a single child. Returns whether it was
    /// found. The root leaf itself is never removed here (the caller handles the
    /// last pane).
    pub fn remove_leaf(&mut self, target: usize) -> bool {
        let PaneTree::Split {
            children, ratios, ..
        } = self
        else {
            return false;
        };
        if let Some(i) = children
            .iter()
            .position(|c| matches!(c, PaneTree::Leaf(s) if *s == target))
        {
            children.remove(i);
            remove_pane_share(ratios, i);
            if children.len() == 1 {
                *self = children.pop().unwrap();
            }
            return true;
        }
        for c in children.iter_mut() {
            if c.remove_leaf(target) {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

impl PaneDirection {
    /// The split axis a directional command operates on.
    pub fn axis(self) -> Axis {
        match self {
            PaneDirection::Left | PaneDirection::Right => Axis::Vertical,
            PaneDirection::Up | PaneDirection::Down => Axis::Horizontal,
        }
    }

    /// Whether the direction points toward higher coordinates / later siblings.
    pub fn forward(self) -> bool {
        matches!(self, PaneDirection::Right | PaneDirection::Down)
    }
}

/// Set pane `d`'s share of the pair it forms with pane `d + 1`, keeping the
/// pair's total constant and both panes at least [`PANE_RATIO_MIN`]. With
/// `snap`, shares close to an even pair split settle exactly there. Returns
/// whether anything changed.
pub fn set_divider_share(ratios: &mut [f32], d: usize, first: f32, snap: bool) -> bool {
    if d + 1 >= ratios.len() {
        return false;
    }
    let pair = ratios[d] + ratios[d + 1];
    let mut first = first;
    if snap && (first - pair / 2.0).abs() < SPLIT_SNAP_EPSILON {
        first = pair / 2.0;
    }
    // A pair squeezed below two minimums degrades to an even split.
    let lo = PANE_RATIO_MIN.min(pair / 2.0);
    let first = first.clamp(lo, pair - lo);
    if (first - ratios[d]).abs() <= f32::EPSILON {
        return false;
    }
    ratios[d] = first;
    ratios[d + 1] = pair - first;
    true
}

/// Make room for a new pane after `at` by halving `at`'s share; when the halves
/// would fall below the minimum, every pane is equalized instead.
pub fn insert_pane_share(ratios: &mut Vec<f32>, at: usize) {
    let half = ratios[at] / 2.0;
    ratios[at] = half;
    ratios.insert(at + 1, half);
    if half < PANE_RATIO_MIN {
        equalize_shares(ratios);
    }
}

/// Remove pane `at`, folding its share into the preceding pane (or the new
/// first pane when `at` was first) so the other panes keep their sizes.
pub fn remove_pane_share(ratios: &mut Vec<f32>, at: usize) {
    if at >= ratios.len() {
        return;
    }
    let freed = ratios.remove(at);
    if let Some(neighbor) = ratios.get_mut(at.saturating_sub(1)) {
        *neighbor += freed;
    }
}

pub fn equalize_shares(ratios: &mut [f32]) {
    let n = ratios.len().max(1) as f32;
    for r in ratios.iter_mut() {
        *r = 1.0 / n;
    }
}

/// Normalize `n` restored shares from an untrusted snapshot: the ratios are
/// kept (rescaled to sum to 1) when they are the right count, finite, and
/// positive; anything else falls back to an even split.
pub fn normalized_shares(n: usize, ratios: &[f32]) -> Vec<f32> {
    let sum: f32 = ratios.iter().sum();
    let usable =
        ratios.len() == n && ratios.iter().all(|x| x.is_finite() && *x > 0.0) && sum > f32::EPSILON;
    if usable {
        ratios.iter().map(|x| *x / sum).collect()
    } else {
        vec![1.0 / n.max(1) as f32; n]
    }
}

/// One leaf's session index and the pixel rectangle it occupies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub session: usize,
    pub rect: Rect,
}

/// Subdivide `rect` across the tree, pushing each leaf's rectangle in
/// depth-first (render/mouse) order. `divider` pixels are reserved between
/// adjacent children, matching the widget tree the caller's view builds.
pub fn collect_pane_rects(node: &PaneTree, rect: Rect, divider: f32, out: &mut Vec<PaneRect>) {
    match node {
        PaneTree::Leaf(session) => out.push(PaneRect {
            session: *session,
            rect,
        }),
        PaneTree::Split {
            axis,
            children,
            ratios,
        } => {
            let n = children.len().max(1);
            let gaps = divider * n.saturating_sub(1) as f32;
            let even = 1.0 / n as f32;
            match axis {
                Axis::Vertical => {
                    let avail = (rect.width - gaps).max(0.0);
                    let mut x = rect.x;
                    for (i, child) in children.iter().enumerate() {
                        let w = avail * ratios.get(i).copied().unwrap_or(even);
                        let child_rect = Rect {
                            x,
                            y: rect.y,
                            width: w,
                            height: rect.height,
                        };
                        collect_pane_rects(child, child_rect, divider, out);
                        x += w + divider;
                    }
                }
                Axis::Horizontal => {
                    let avail = (rect.height - gaps).max(0.0);
                    let mut y = rect.y;
                    for (i, child) in children.iter().enumerate() {
                        let h = avail * ratios.get(i).copied().unwrap_or(even);
                        let child_rect = Rect {
                            x: rect.x,
                            y,
                            width: rect.width,
                            height: h,
                        };
                        collect_pane_rects(child, child_rect, divider, out);
                        y += h + divider;
                    }
                }
            }
        }
    }
}

/// The axis and pixel rectangle of the split node reached by `path`, within an
/// outer `root` rectangle. Used to convert a divider drag into a fraction of
/// its own node's extent. Returns `None` if the path does not land on a `Split`.
pub fn split_node_rect(
    root: &PaneTree,
    path: &[usize],
    rect: Rect,
    divider: f32,
) -> Option<(Axis, Rect)> {
    match root {
        PaneTree::Leaf(_) => None,
        PaneTree::Split {
            axis,
            children,
            ratios,
        } => {
            let Some((&head, tail)) = path.split_first() else {
                return Some((*axis, rect));
            };
            let n = children.len().max(1);
            let gaps = divider * n.saturating_sub(1) as f32;
            let even = 1.0 / n as f32;
            // Walk to child `head`, accumulating its offset along the axis.
            let mut offset = 0.0;
            for i in 0..head {
                let share = ratios.get(i).copied().unwrap_or(even);
                offset += match axis {
                    Axis::Vertical => (rect.width - gaps).max(0.0) * share,
                    Axis::Horizontal => (rect.height - gaps).max(0.0) * share,
                } + divider;
            }
            let share = ratios.get(head).copied().unwrap_or(even);
            let child_rect = match axis {
                Axis::Vertical => Rect {
                    x: rect.x + offset,
                    y: rect.y,
                    width: (rect.width - gaps).max(0.0) * share,
                    height: rect.height,
                },
                Axis::Horizontal => Rect {
                    x: rect.x,
                    y: rect.y + offset,
                    width: rect.width,
                    height: (rect.height - gaps).max(0.0) * share,
                },
            };
            split_node_rect(children.get(head)?, tail, child_rect, divider)
        }
    }
}

/// The pane physically adjacent to `active` in `direction` (tmux-style spatial
/// navigation across nesting), given every leaf's laid-out rectangle. Nearest
/// along the direction wins; ties break toward the larger perpendicular
/// overlap. No wrap at the edges.
pub fn directional_focus_target(
    rects: &[PaneRect],
    active: usize,
    direction: PaneDirection,
) -> Option<usize> {
    let cur = rects.iter().find(|p| p.session == active)?;
    let c = cur.rect;
    let eps = 1.0;
    rects
        .iter()
        .filter(|p| p.session != active)
        .filter_map(|p| {
            let r = p.rect;
            // Candidate must sit on `direction`'s side and overlap the
            // focused pane along the perpendicular axis.
            let (on_side, gap, perp_overlap) = match direction {
                PaneDirection::Left => (
                    r.x + r.width <= c.x + eps,
                    c.x - (r.x + r.width),
                    (r.y + r.height).min(c.y + c.height) - r.y.max(c.y),
                ),
                PaneDirection::Right => (
                    r.x >= c.x + c.width - eps,
                    r.x - (c.x + c.width),
                    (r.y + r.height).min(c.y + c.height) - r.y.max(c.y),
                ),
                PaneDirection::Up => (
                    r.y + r.height <= c.y + eps,
                    c.y - (r.y + r.height),
                    (r.x + r.width).min(c.x + c.width) - r.x.max(c.x),
                ),
                PaneDirection::Down => (
                    r.y >= c.y + c.height - eps,
                    r.y - (c.y + c.height),
                    (r.x + r.width).min(c.x + c.width) - r.x.max(c.x),
                ),
            };
            (on_side && perp_overlap > 0.0).then_some((p.session, gap.max(0.0), perp_overlap))
        })
        // Nearest along the direction; ties broken by the larger overlap.
        .min_by(|a, b| a.1.total_cmp(&b.1).then(b.2.total_cmp(&a.2)))
        .map(|(session, _, _)| session)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIVIDER: f32 = 6.0;

    #[test]
    fn split_leaf_joins_same_axis_and_nests_across_axes() {
        // A lone leaf becomes a two-child split of the requested axis.
        let mut t = PaneTree::Leaf(0);
        assert!(t.split_leaf(0, Axis::Vertical, 1));
        assert_eq!(t.leaves(), vec![0, 1]);
        // Splitting an existing sibling along the SAME axis joins it flat.
        assert!(t.split_leaf(1, Axis::Vertical, 2));
        assert_eq!(t.leaves(), vec![0, 1, 2]);
        assert_eq!(t.leaf_count(), 3);
        // Splitting along the OTHER axis nests a new split at that leaf.
        assert!(t.split_leaf(1, Axis::Horizontal, 3));
        assert_eq!(t.leaves(), vec![0, 1, 3, 2]);
        // The nested node really is horizontal.
        assert_eq!(t.path_to_session(3), Some(vec![1, 1]));
        // Splitting a session that does not exist is a no-op.
        assert!(!t.split_leaf(99, Axis::Vertical, 4));
    }

    #[test]
    fn activation_focuses_a_visible_session_without_changing_the_tree() {
        let mut tree = PaneTree::Split {
            axis: Axis::Vertical,
            children: vec![
                PaneTree::Leaf(0),
                PaneTree::Split {
                    axis: Axis::Horizontal,
                    children: vec![PaneTree::Leaf(1), PaneTree::Leaf(2)],
                    ratios: vec![0.35, 0.65],
                },
            ],
            ratios: vec![0.4, 0.6],
        };
        let before = tree.clone();

        assert!(tree.focus_or_replace_session(0, 2));
        assert_eq!(tree, before);
        assert_eq!(tree.leaves().iter().filter(|&&s| s == 2).count(), 1);
    }

    #[test]
    fn activation_replaces_only_the_focused_leaf_for_a_hidden_session() {
        let mut tree = PaneTree::Split {
            axis: Axis::Vertical,
            children: vec![
                PaneTree::Leaf(0),
                PaneTree::Split {
                    axis: Axis::Horizontal,
                    children: vec![PaneTree::Leaf(1), PaneTree::Leaf(2)],
                    ratios: vec![0.35, 0.65],
                },
            ],
            ratios: vec![0.4, 0.6],
        };

        assert!(tree.focus_or_replace_session(1, 3));
        assert_eq!(
            tree,
            PaneTree::Split {
                axis: Axis::Vertical,
                children: vec![
                    PaneTree::Leaf(0),
                    PaneTree::Split {
                        axis: Axis::Horizontal,
                        children: vec![PaneTree::Leaf(3), PaneTree::Leaf(2)],
                        ratios: vec![0.35, 0.65],
                    },
                ],
                ratios: vec![0.4, 0.6],
            }
        );
        assert_eq!(tree.leaves(), vec![0, 3, 2]);
        assert_eq!(tree.leaves().iter().filter(|&&s| s == 3).count(), 1);
    }

    #[test]
    fn activation_preserves_single_pane_behavior_and_rejects_missing_focus() {
        let mut single = PaneTree::Leaf(0);
        assert!(single.focus_or_replace_session(0, 4));
        assert_eq!(single, PaneTree::Leaf(4));

        let before = single.clone();
        assert!(!single.focus_or_replace_session(99, 5));
        assert_eq!(single, before);
    }

    #[test]
    fn remove_leaf_folds_share_and_collapses_single_child_parents() {
        // V[ 0, H[1, 3], 2 ]  — remove 3 collapses the H node into Leaf(1).
        let mut t = PaneTree::Split {
            axis: Axis::Vertical,
            children: vec![
                PaneTree::Leaf(0),
                PaneTree::Split {
                    axis: Axis::Horizontal,
                    children: vec![PaneTree::Leaf(1), PaneTree::Leaf(3)],
                    ratios: vec![0.5, 0.5],
                },
                PaneTree::Leaf(2),
            ],
            ratios: vec![0.3, 0.4, 0.3],
        };
        assert!(t.remove_leaf(3));
        assert_eq!(t.leaves(), vec![0, 1, 2]);
        // The middle child is now a plain leaf, not a split.
        assert_eq!(t.path_to_session(1), Some(vec![1]));
        // Removing down to one child collapses the whole tree to a leaf.
        assert!(t.remove_leaf(0));
        assert!(t.remove_leaf(1));
        assert_eq!(t, PaneTree::Leaf(2));
        // A root leaf is never removed here (the caller handles the last pane).
        assert!(!t.remove_leaf(2));
    }

    #[test]
    fn collect_pane_rects_partitions_the_area_along_each_axis() {
        let area = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0 + DIVIDER,
            height: 60.0,
        };
        let tree = PaneTree::Split {
            axis: Axis::Vertical,
            children: vec![PaneTree::Leaf(0), PaneTree::Leaf(1)],
            ratios: vec![0.5, 0.5],
        };
        let mut rects = Vec::new();
        collect_pane_rects(&tree, area, DIVIDER, &mut rects);
        assert_eq!(rects.len(), 2);
        // Two 50-wide panes with a DIVIDER gap between them.
        assert_eq!(rects[0].session, 0);
        assert!((rects[0].rect.width - 50.0).abs() < 1e-3);
        assert!((rects[1].rect.x - (50.0 + DIVIDER)).abs() < 1e-3);
        assert!((rects[1].rect.width - 50.0).abs() < 1e-3);
        // Every pane spans the full perpendicular extent.
        assert!(rects.iter().all(|p| (p.rect.height - 60.0).abs() < 1e-3));
    }

    #[test]
    fn divider_shares_clamp_to_the_pane_minimum_and_snap_when_close() {
        let mut ratios = vec![0.5, 0.5];
        // Snap: near an even pair split settles exactly at half the pair.
        assert!(set_divider_share(
            &mut ratios,
            0,
            0.5 + SPLIT_SNAP_EPSILON * 2.0,
            true
        ));
        assert!(set_divider_share(
            &mut ratios,
            0,
            0.5 + SPLIT_SNAP_EPSILON * 0.5,
            true
        ));
        assert_eq!(ratios, vec![0.5, 0.5]);
        // Clamping: neither pane of the pair may go below the minimum.
        assert!(set_divider_share(&mut ratios, 0, 0.0, false));
        assert!((ratios[0] - PANE_RATIO_MIN).abs() < 1e-6);
        assert!((ratios[1] - (1.0 - PANE_RATIO_MIN)).abs() < 1e-6);
        // Only the pair around the divider moves; other panes are untouched.
        let mut three = vec![0.25, 0.25, 0.5];
        assert!(set_divider_share(&mut three, 0, 0.3, false));
        assert!((three[0] - 0.3).abs() < 1e-6);
        assert!((three[1] - 0.2).abs() < 1e-6);
        assert!((three[2] - 0.5).abs() < 1e-6);
        // Out-of-range divider index is rejected.
        assert!(!set_divider_share(&mut three, 2, 0.4, false));
    }

    #[test]
    fn pane_shares_split_in_half_on_insert_and_refold_on_remove() {
        let mut ratios = vec![0.5, 0.5];
        insert_pane_share(&mut ratios, 1);
        assert_eq!(ratios, vec![0.5, 0.25, 0.25]);
        // Removing the middle pane folds its share into the previous one.
        remove_pane_share(&mut ratios, 1);
        assert_eq!(ratios, vec![0.75, 0.25]);
        // Removing the first pane folds forward into the new first.
        remove_pane_share(&mut ratios, 0);
        assert_eq!(ratios, vec![1.0]);
        // Splitting a pane too thin to halve equalizes the whole row instead.
        let mut thin = vec![1.0 - PANE_RATIO_MIN, PANE_RATIO_MIN];
        insert_pane_share(&mut thin, 1);
        assert_eq!(thin.len(), 3);
        assert!(thin.iter().all(|r| (r - 1.0 / 3.0).abs() < 1e-6));
    }

    #[test]
    fn normalized_shares_rescales_valid_ratios_and_evens_out_bad_ones() {
        assert_eq!(normalized_shares(2, &[1.0, 3.0]), vec![0.25, 0.75]);
        // Wrong count, non-finite, or non-positive entries fall back to even.
        assert_eq!(normalized_shares(2, &[1.0]), vec![0.5, 0.5]);
        assert_eq!(normalized_shares(2, &[f32::NAN, 1.0]), vec![0.5, 0.5]);
        assert_eq!(normalized_shares(2, &[-1.0, 2.0]), vec![0.5, 0.5]);
    }

    #[test]
    fn directional_focus_picks_the_nearest_overlapping_pane() {
        // [0][1] side by side, 2 below spanning the full width.
        let rects = vec![
            PaneRect {
                session: 0,
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 50.0,
                    height: 50.0,
                },
            },
            PaneRect {
                session: 1,
                rect: Rect {
                    x: 56.0,
                    y: 0.0,
                    width: 50.0,
                    height: 50.0,
                },
            },
            PaneRect {
                session: 2,
                rect: Rect {
                    x: 0.0,
                    y: 56.0,
                    width: 106.0,
                    height: 50.0,
                },
            },
        ];
        assert_eq!(
            directional_focus_target(&rects, 0, PaneDirection::Right),
            Some(1)
        );
        assert_eq!(
            directional_focus_target(&rects, 1, PaneDirection::Left),
            Some(0)
        );
        assert_eq!(
            directional_focus_target(&rects, 0, PaneDirection::Down),
            Some(2)
        );
        assert_eq!(
            directional_focus_target(&rects, 2, PaneDirection::Up),
            Some(0)
        );
        // No wrap at the edges.
        assert_eq!(directional_focus_target(&rects, 0, PaneDirection::Left), None);
        assert_eq!(directional_focus_target(&rects, 2, PaneDirection::Down), None);
    }
}
