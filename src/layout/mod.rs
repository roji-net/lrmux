//! Pane layout: binary split tree + geometry reflow.
//!
//! A window's panes are arranged in a tree of horizontal (`{…}`, left|right)
//! and vertical (`[…]`, top/bottom) splits. `reflow` walks the tree against a
//! window size and produces a `(pane_id, Rect)` for each leaf, leaving a
//! 1-cell gap between siblings for the border (tmux-compatible).
//!
//! The tmux layout string (`checksum,WxH,X,Y…`) is produced from the same
//! tree so -CC / iTerm2 can render native pane chrome.

/// Split axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    /// Left | right — tmux `split-window -h`, layout braces `{…}`.
    Horizontal,
    /// Top / bottom — tmux `split-window -v`, layout brackets `[…]`.
    Vertical,
}

/// Direction for `select-pane`-style focus movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Up,
    Down,
    Left,
    Right,
}

/// Pixel-cell rectangle inside the window (origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, cols: u16, rows: u16) -> Self {
        Self { x, y, cols, rows }
    }
}

#[derive(Debug, Clone)]
enum Node {
    Leaf {
        pane_id: u32,
    },
    Split {
        dir: SplitDir,
        /// Fraction of the usable space (after the 1-cell gap) given to
        /// `first`. Clamped to (0, 1) on insert; default 0.5.
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// Window layout tree.
#[derive(Debug, Clone)]
pub struct Layout {
    root: Node,
}

impl Layout {
    /// Single-pane window.
    pub fn single(pane_id: u32) -> Self {
        Self {
            root: Node::Leaf { pane_id },
        }
    }

    /// Split the leaf `target` along `dir`, inserting `new_pane` as the
    /// second child. Returns false if `target` is not a leaf in the tree.
    pub fn split(&mut self, target: u32, dir: SplitDir, new_pane: u32, ratio: f64) -> bool {
        let ratio = ratio.clamp(0.05, 0.95);
        split_node(&mut self.root, target, dir, new_pane, ratio)
    }

    /// Remove a leaf. If it was one side of a split, the sibling is promoted.
    /// Refuses to remove the last remaining pane. Returns false if missing.
    pub fn remove(&mut self, pane_id: u32) -> bool {
        if matches!(&self.root, Node::Leaf { pane_id: id } if *id == pane_id) {
            return false; // never empty the window here
        }
        remove_node(&mut self.root, pane_id)
    }

    /// True if `pane_id` is a leaf in this layout.
    pub fn contains(&self, pane_id: u32) -> bool {
        contains_node(&self.root, pane_id)
    }

    /// All pane ids in tree order (left/top first).
    pub fn pane_ids(&self) -> Vec<u32> {
        let mut out = Vec::new();
        collect_ids(&self.root, &mut out);
        out
    }

    pub fn pane_count(&self) -> usize {
        self.pane_ids().len()
    }

    /// Compute absolute geometry for every leaf given the window size.
    pub fn reflow(&self, cols: u16, rows: u16) -> Vec<(u32, Rect)> {
        let mut out = Vec::new();
        reflow_node(
            &self.root,
            Rect::new(0, 0, cols.max(1), rows.max(1)),
            &mut out,
        );
        out
    }

    /// tmux layout string: `checksum,body` where body is the recursive
    /// `WxH,X,Y…` form iTerm2's TmuxLayoutParser expects (width=cols first).
    pub fn to_tmux_str(&self, cols: u16, rows: u16) -> String {
        let rects = self.reflow(cols, rows);
        let body = format_node(&self.root, &rects);
        let csum = layout_checksum(&body);
        format!("{csum:04x},{body}")
    }

    /// Cycle presets: even-horizontal → even-vertical → tiled → …
    /// `kind` is the current preset index; returns `(new_layout, new_kind)`.
    pub fn cycle_preset(ids: &[u32], kind: u8) -> (Self, u8) {
        let next = (kind + 1) % 3;
        let layout = match next {
            0 => even_rebuild(ids, SplitDir::Horizontal),
            1 => even_rebuild(ids, SplitDir::Vertical),
            _ => tiled_rebuild(ids),
        };
        (layout, next)
    }

    /// Pane adjacent to `from` in `dir` (tmux `select-pane -UDLR`).
    /// Prefers overlapping neighbors closest along the travel axis.
    pub fn neighbor(&self, from: u32, dir: FocusDir, cols: u16, rows: u16) -> Option<u32> {
        let rects = self.reflow(cols, rows);
        let src = leaf_rect(&rects, from);
        if !rects.iter().any(|(id, _)| *id == from) {
            return None;
        }
        let src_cx = src.x as i32 + src.cols as i32 / 2;
        let src_cy = src.y as i32 + src.rows as i32 / 2;
        let mut best: Option<(u32, i32, i32)> = None; // id, primary, secondary
        for &(id, r) in &rects {
            if id == from {
                continue;
            }
            let cx = r.x as i32 + r.cols as i32 / 2;
            let cy = r.y as i32 + r.rows as i32 / 2;
            let (ok, primary, secondary) = match dir {
                FocusDir::Left => {
                    let edge = (r.x + r.cols) as i32;
                    let primary = src.x as i32 - edge;
                    (
                        primary >= 1 && ranges_overlap(src.y, src.rows, r.y, r.rows),
                        primary,
                        (src_cy - cy).abs(),
                    )
                }
                FocusDir::Right => {
                    let primary = r.x as i32 - (src.x + src.cols) as i32;
                    (
                        primary >= 1 && ranges_overlap(src.y, src.rows, r.y, r.rows),
                        primary,
                        (src_cy - cy).abs(),
                    )
                }
                FocusDir::Up => {
                    let edge = (r.y + r.rows) as i32;
                    let primary = src.y as i32 - edge;
                    (
                        primary >= 1 && ranges_overlap(src.x, src.cols, r.x, r.cols),
                        primary,
                        (src_cx - cx).abs(),
                    )
                }
                FocusDir::Down => {
                    let primary = r.y as i32 - (src.y + src.rows) as i32;
                    (
                        primary >= 1 && ranges_overlap(src.x, src.cols, r.x, r.cols),
                        primary,
                        (src_cx - cx).abs(),
                    )
                }
            };
            if !ok {
                continue;
            }
            match best {
                None => best = Some((id, primary, secondary)),
                Some((_, bp, bs)) if primary < bp || (primary == bp && secondary < bs) => {
                    best = Some((id, primary, secondary));
                }
                _ => {}
            }
        }
        best.map(|(id, _, _)| id)
    }
}

fn ranges_overlap(a0: u16, a_len: u16, b0: u16, b_len: u16) -> bool {
    let a1 = a0.saturating_add(a_len);
    let b1 = b0.saturating_add(b_len);
    a0 < b1 && b0 < a1
}

/// Balanced binary tree along `dir` with ratios matching leaf counts.
fn even_rebuild(ids: &[u32], dir: SplitDir) -> Layout {
    if ids.is_empty() {
        return Layout::single(0);
    }
    Layout {
        root: even_node(ids, dir),
    }
}

fn even_node(ids: &[u32], dir: SplitDir) -> Node {
    debug_assert!(!ids.is_empty());
    if ids.len() == 1 {
        return Node::Leaf { pane_id: ids[0] };
    }
    let mid = ids.len() / 2;
    let mid = mid.max(1);
    let (left, right) = ids.split_at(mid);
    Node::Split {
        dir,
        ratio: left.len() as f64 / ids.len() as f64,
        first: Box::new(even_node(left, dir)),
        second: Box::new(even_node(right, dir)),
    }
}

/// Grid-ish: rows of even-horizontal, stacked even-vertical.
fn tiled_rebuild(ids: &[u32]) -> Layout {
    if ids.is_empty() {
        return Layout::single(0);
    }
    if ids.len() == 1 {
        return Layout::single(ids[0]);
    }
    let cols = (ids.len() as f64).sqrt().ceil() as usize;
    let cols = cols.max(1);
    let row_nodes: Vec<Node> = ids
        .chunks(cols)
        .map(|row| even_node(row, SplitDir::Horizontal))
        .collect();
    Layout {
        root: stack_nodes(&row_nodes, SplitDir::Vertical),
    }
}

fn stack_nodes(nodes: &[Node], dir: SplitDir) -> Node {
    debug_assert!(!nodes.is_empty());
    if nodes.len() == 1 {
        return nodes[0].clone();
    }
    let mid = (nodes.len() / 2).max(1);
    Node::Split {
        dir,
        ratio: mid as f64 / nodes.len() as f64,
        first: Box::new(stack_nodes(&nodes[..mid], dir)),
        second: Box::new(stack_nodes(&nodes[mid..], dir)),
    }
}

fn split_node(node: &mut Node, target: u32, dir: SplitDir, new_pane: u32, ratio: f64) -> bool {
    match node {
        Node::Leaf { pane_id } if *pane_id == target => {
            *node = Node::Split {
                dir,
                ratio,
                first: Box::new(Node::Leaf { pane_id: target }),
                second: Box::new(Node::Leaf { pane_id: new_pane }),
            };
            true
        }
        Node::Leaf { .. } => false,
        Node::Split { first, second, .. } => {
            split_node(first, target, dir, new_pane, ratio)
                || split_node(second, target, dir, new_pane, ratio)
        }
    }
}

fn remove_node(node: &mut Node, pane_id: u32) -> bool {
    match node {
        Node::Leaf { .. } => false,
        Node::Split { first, second, .. } => {
            if matches!(first.as_ref(), Node::Leaf { pane_id: id } if *id == pane_id) {
                *node = *second.clone();
                return true;
            }
            if matches!(second.as_ref(), Node::Leaf { pane_id: id } if *id == pane_id) {
                *node = *first.clone();
                return true;
            }
            remove_node(first, pane_id) || remove_node(second, pane_id)
        }
    }
}

fn contains_node(node: &Node, pane_id: u32) -> bool {
    match node {
        Node::Leaf { pane_id: id } => *id == pane_id,
        Node::Split { first, second, .. } => {
            contains_node(first, pane_id) || contains_node(second, pane_id)
        }
    }
}

fn collect_ids(node: &Node, out: &mut Vec<u32>) {
    match node {
        Node::Leaf { pane_id } => out.push(*pane_id),
        Node::Split { first, second, .. } => {
            collect_ids(first, out);
            collect_ids(second, out);
        }
    }
}

fn reflow_node(node: &Node, area: Rect, out: &mut Vec<(u32, Rect)>) {
    match node {
        Node::Leaf { pane_id } => out.push((*pane_id, area)),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *dir, *ratio);
            reflow_node(first, a, out);
            reflow_node(second, b, out);
        }
    }
}

/// Partition `area` along `dir`, leaving a 1-cell gap for the border when
/// there is room. Both children get at least 1 cell when possible.
fn split_rect(area: Rect, dir: SplitDir, ratio: f64) -> (Rect, Rect) {
    match dir {
        SplitDir::Horizontal => {
            if area.cols <= 2 {
                // Degenerate: no gap, hand 1 col to first if possible.
                let left = area
                    .cols
                    .saturating_sub(1)
                    .max(if area.cols > 0 { 1 } else { 0 });
                let right = area.cols.saturating_sub(left);
                (
                    Rect::new(area.x, area.y, left, area.rows),
                    Rect::new(area.x.saturating_add(left), area.y, right, area.rows),
                )
            } else {
                let usable = area.cols - 1; // gap
                let mut left = ((usable as f64) * ratio).round() as u16;
                left = left.clamp(1, usable - 1);
                let right = usable - left;
                (
                    Rect::new(area.x, area.y, left, area.rows),
                    Rect::new(area.x + left + 1, area.y, right, area.rows),
                )
            }
        }
        SplitDir::Vertical => {
            if area.rows <= 2 {
                let top = area
                    .rows
                    .saturating_sub(1)
                    .max(if area.rows > 0 { 1 } else { 0 });
                let bottom = area.rows.saturating_sub(top);
                (
                    Rect::new(area.x, area.y, area.cols, top),
                    Rect::new(area.x, area.y.saturating_add(top), area.cols, bottom),
                )
            } else {
                let usable = area.rows - 1;
                let mut top = ((usable as f64) * ratio).round() as u16;
                top = top.clamp(1, usable - 1);
                let bottom = usable - top;
                (
                    Rect::new(area.x, area.y, area.cols, top),
                    Rect::new(area.x, area.y + top + 1, area.cols, bottom),
                )
            }
        }
    }
}

fn leaf_rect(rects: &[(u32, Rect)], pane_id: u32) -> Rect {
    rects
        .iter()
        .find(|(id, _)| *id == pane_id)
        .map(|(_, r)| *r)
        .unwrap_or(Rect::new(0, 0, 1, 1))
}

fn format_node(node: &Node, rects: &[(u32, Rect)]) -> String {
    match node {
        Node::Leaf { pane_id } => {
            let r = leaf_rect(rects, *pane_id);
            format!("{}x{},{},{},{}", r.cols, r.rows, r.x, r.y, pane_id)
        }
        Node::Split {
            dir, first, second, ..
        } => {
            // Container covers the union of both children plus the gap.
            let r1 = node_outer_rect(first, rects);
            let r2 = node_outer_rect(second, rects);
            let (x, y, cols, rows) = match dir {
                SplitDir::Horizontal => {
                    let x = r1.x.min(r2.x);
                    let y = r1.y.min(r2.y);
                    let cols = r2.x + r2.cols - x;
                    let rows = r1.rows.max(r2.rows);
                    (x, y, cols, rows)
                }
                SplitDir::Vertical => {
                    let x = r1.x.min(r2.x);
                    let y = r1.y.min(r2.y);
                    let cols = r1.cols.max(r2.cols);
                    let rows = r2.y + r2.rows - y;
                    (x, y, cols, rows)
                }
            };
            let left = format_node(first, rects);
            let right = format_node(second, rects);
            match dir {
                SplitDir::Horizontal => {
                    format!("{cols}x{rows},{x},{y}{{{left},{right}}}")
                }
                SplitDir::Vertical => {
                    format!("{cols}x{rows},{x},{y}[{left},{right}]")
                }
            }
        }
    }
}

fn node_outer_rect(node: &Node, rects: &[(u32, Rect)]) -> Rect {
    match node {
        Node::Leaf { pane_id } => leaf_rect(rects, *pane_id),
        Node::Split {
            first, second, dir, ..
        } => {
            let a = node_outer_rect(first, rects);
            let b = node_outer_rect(second, rects);
            match dir {
                SplitDir::Horizontal => Rect::new(
                    a.x.min(b.x),
                    a.y.min(b.y),
                    b.x + b.cols - a.x.min(b.x),
                    a.rows.max(b.rows),
                ),
                SplitDir::Vertical => Rect::new(
                    a.x.min(b.x),
                    a.y.min(b.y),
                    a.cols.max(b.cols),
                    b.y + b.rows - a.y.min(b.y),
                ),
            }
        }
    }
}

/// tmux `layout_checksum`: 16-bit rotate-and-add over the layout body.
fn layout_checksum(layout: &str) -> u16 {
    let mut csum: u16 = 0;
    for b in layout.bytes() {
        csum = (csum >> 1) + ((csum & 1) << 15) + u16::from(b);
    }
    csum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane_fills_window() {
        let layout = Layout::single(1);
        assert_eq!(layout.reflow(80, 24), vec![(1, Rect::new(0, 0, 80, 24))]);
        let s = layout.to_tmux_str(80, 24);
        assert!(s.ends_with(",80x24,0,0,1"), "{s}");
        let body = s.split_once(',').unwrap().1;
        assert_eq!(&s[..4], format!("{:04x}", layout_checksum(body)));
    }

    #[test]
    fn horizontal_split_leaves_gap() {
        let mut layout = Layout::single(1);
        assert!(layout.split(1, SplitDir::Horizontal, 2, 0.5));
        let rects = layout.reflow(81, 24);
        // usable=80, half=40 each, gap at col 40
        assert_eq!(rects[0], (1, Rect::new(0, 0, 40, 24)));
        assert_eq!(rects[1], (2, Rect::new(41, 0, 40, 24)));
        let s = layout.to_tmux_str(81, 24);
        assert!(s.contains('{'), "{s}");
        assert!(s.contains("40x24,0,0,1"), "{s}");
        assert!(s.contains("40x24,41,0,2"), "{s}");
    }

    #[test]
    fn vertical_split_leaves_gap() {
        let mut layout = Layout::single(1);
        assert!(layout.split(1, SplitDir::Vertical, 2, 0.5));
        let rects = layout.reflow(80, 25);
        assert_eq!(rects[0], (1, Rect::new(0, 0, 80, 12)));
        assert_eq!(rects[1], (2, Rect::new(0, 13, 80, 12)));
        let s = layout.to_tmux_str(80, 25);
        assert!(s.contains('['), "{s}");
    }

    #[test]
    fn nested_splits_and_remove() {
        let mut layout = Layout::single(1);
        assert!(layout.split(1, SplitDir::Horizontal, 2, 0.5));
        assert!(layout.split(2, SplitDir::Vertical, 3, 0.5));
        assert_eq!(layout.pane_count(), 3);
        assert!(layout.contains(3));
        assert!(layout.remove(3));
        assert_eq!(layout.pane_ids(), vec![1, 2]);
        assert!(layout.remove(2));
        assert_eq!(layout.pane_ids(), vec![1]);
        assert!(!layout.remove(1)); // last pane stays
    }

    #[test]
    fn split_missing_target_fails() {
        let mut layout = Layout::single(1);
        assert!(!layout.split(99, SplitDir::Horizontal, 2, 0.5));
        assert_eq!(layout.pane_count(), 1);
    }

    #[test]
    fn uneven_ratio() {
        let mut layout = Layout::single(1);
        assert!(layout.split(1, SplitDir::Horizontal, 2, 0.25));
        let rects = layout.reflow(81, 10);
        let (_, a) = rects[0];
        let (_, b) = rects[1];
        assert_eq!(a.cols + 1 + b.cols, 81);
        assert!(a.cols < b.cols, "{a:?} {b:?}");
    }

    #[test]
    fn cycle_preset_even_and_tiled() {
        let ids = [1u32, 2, 3, 4];
        let (h, k0) = Layout::cycle_preset(&ids, 2); // → even-horizontal
        assert_eq!(k0, 0);
        assert_eq!(h.pane_ids(), ids);
        let rects = h.reflow(81, 24);
        assert_eq!(rects.len(), 4);
        // All share the same row band in an even-horizontal layout.
        assert!(rects.iter().all(|(_, r)| r.y == 0 && r.rows == 24));

        let (v, k1) = Layout::cycle_preset(&ids, 0); // → even-vertical
        assert_eq!(k1, 1);
        let rects = v.reflow(80, 25);
        assert!(rects.iter().all(|(_, r)| r.x == 0 && r.cols == 80));

        let (t, k2) = Layout::cycle_preset(&ids, 1); // → tiled
        assert_eq!(k2, 2);
        assert_eq!(t.pane_count(), 4);
    }

    #[test]
    fn neighbor_cardinal_and_nested() {
        let mut layout = Layout::single(1);
        assert!(layout.split(1, SplitDir::Horizontal, 2, 0.5)); // 1 | 2
        assert_eq!(layout.neighbor(1, FocusDir::Right, 81, 24), Some(2));
        assert_eq!(layout.neighbor(2, FocusDir::Left, 81, 24), Some(1));
        assert_eq!(layout.neighbor(1, FocusDir::Left, 81, 24), None);
        assert_eq!(layout.neighbor(1, FocusDir::Up, 81, 24), None);

        assert!(layout.split(2, SplitDir::Vertical, 3, 0.5)); // 1 | {2/3}
        assert_eq!(layout.neighbor(1, FocusDir::Right, 81, 25), Some(2));
        assert_eq!(layout.neighbor(2, FocusDir::Down, 81, 25), Some(3));
        assert_eq!(layout.neighbor(3, FocusDir::Up, 81, 25), Some(2));
        assert_eq!(layout.neighbor(3, FocusDir::Left, 81, 25), Some(1));
    }
}
