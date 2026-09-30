//! Workspaces: named tabs, each owning a split-pane tree.
//!
//! A workspace is one independent split layout (`Node` tree over `Pane`s).
//! All workspaces keep running in the background, so the inbox menu can show
//! per-workspace status (alive panes, finished shells, last tool result).

use std::collections::HashMap;

use crossbeam_channel::Sender;
use ratatui::layout::Rect;

use crate::layout::TreeLayout;
use crate::pty::Pane;

/// Horizontal = stacked top/bottom; Vertical = side-by-side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// Focus/resize direction on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Binary split tree. Leaves reference pane ids (unique within a workspace).
/// `ratio` is the first child's share of a split (0.1..=0.9).
#[derive(Debug, Clone)]
pub enum Node {
    Pane(usize),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// Leaf ids in visual order.
    pub fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            Node::Pane(id) => out.push(*id),
            Node::Split { first, second, .. } => {
                first.leaves(out);
                second.leaves(out);
            }
        }
    }

    /// Replace leaf `target` with `replacement`. True when replaced.
    fn replace(&mut self, target: usize, replacement: Node) -> bool {
        match self {
            Node::Pane(id) if *id == target => {
                *self = replacement;
                true
            }
            Node::Pane(_) => false,
            Node::Split { first, second, .. } => {
                first.replace(target, replacement.clone()) || second.replace(target, replacement)
            }
        }
    }

    /// Remove leaf `target`; collapses the parent split onto the surviving
    /// side. Returns true when found. Caller checks `is_empty_opt` for the
    /// last-pane-removed case.
    fn remove(node: &mut Option<Node>, target: usize) -> bool {
        match node {
            None => false,
            Some(Node::Pane(id)) if *id == target => {
                *node = None;
                true
            }
            Some(Node::Pane(_)) => false,
            Some(Node::Split { .. }) => {
                let Some(Node::Split {
                    dir,
                    ratio,
                    first,
                    second,
                }) = node.take()
                else {
                    unreachable!()
                };
                let mut a = Some(*first);
                let mut b = Some(*second);
                let found = Node::remove(&mut a, target) || Node::remove(&mut b, target);
                if !found {
                    *node = Some(Node::Split {
                        dir,
                        ratio,
                        first: Box::new(a.unwrap()),
                        second: Box::new(b.unwrap()),
                    });
                    return false;
                }
                match (a, b) {
                    (Some(x), Some(y)) => {
                        *node = Some(Node::Split {
                            dir,
                            ratio,
                            first: Box::new(x),
                            second: Box::new(y),
                        })
                    }
                    (Some(x), None) => *node = Some(x),
                    (None, Some(y)) => *node = Some(y),
                    (None, None) => *node = None,
                }
                true
            }
        }
    }

    fn is_empty_opt(opt: &Option<Node>) -> bool {
        opt.is_none()
    }

    /// Persisted shape of this subtree (pane ids are dropped).
    pub fn tree_layout(&self) -> TreeLayout {
        match self {
            Node::Pane(_) => TreeLayout::Pane,
            Node::Split {
                dir,
                ratio,
                first,
                second,
            } => TreeLayout::split(*dir, *ratio, first.tree_layout(), second.tree_layout()),
        }
    }

    /// Rebuild a tree from its saved shape, spawning one pane per leaf with
    /// ids `next_id..` (returned alongside the tree). Each leaf pumps on its
    /// own tokio blocking thread via `rt`. Leaves consume `dirs` in visual
    /// order (same order as [`Node::leaves`]): each pane starts in its saved
    /// directory, falling back to the process directory when the entry is
    /// missing or gone (`spawn_in` handles that).
    fn from_layout(
        layout: &TreeLayout,
        panes: &mut Vec<Pane>,
        next_id: &mut usize,
        dirs: &[std::path::PathBuf],
        dir_idx: &mut usize,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &Sender<()>,
    ) -> Result<Node, String> {
        match layout {
            TreeLayout::Pane => {
                let id = *next_id;
                *next_id += 1;
                let cwd = dirs.get(*dir_idx).cloned().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                });
                *dir_idx += 1;
                panes.push(Pane::spawn_in(id, shell, scrollback, rt, wake, &cwd)?);
                Ok(Node::Pane(id))
            }
            TreeLayout::Split { first, second, .. } => {
                let a =
                    Node::from_layout(first, panes, next_id, dirs, dir_idx, shell, scrollback, rt, wake)?;
                let b =
                    Node::from_layout(second, panes, next_id, dirs, dir_idx, shell, scrollback, rt, wake)?;
                Ok(Node::Split {
                    dir: layout.direction(),
                    ratio: layout.ratio(),
                    first: Box::new(a),
                    second: Box::new(b),
                })
            }
        }
    }
}

/// One named tab: split tree + panes + status for the inbox.
pub struct Workspace {
    pub name: String,
    panes: Vec<Pane>,
    root: Option<Node>,
    pub focused: usize,
    next_id: usize,
    /// Last finished thing: tool result or exited shell. Shown in inbox.
    pub last_done: String,
    /// Total PTY bytes received (background activity indicator).
    pub activity: u64,
    /// Activity count last acknowledged (reset when workspace focused).
    seen: u64,
    /// Last drawn pane rects (pane id -> area), refreshed by ui every frame.
    /// Used for directional focus; empty until the first draw.
    /// While zoomed this holds only the maximized pane (what is on screen).
    rects: HashMap<usize, Rect>,
    /// Last fully-tiled pane rects, refreshed by ui every frame even while
    /// zoomed. Directional focus reads these so one keypress both restores
    /// a maximized pane and moves (zoomed draw rects alone have no neighbor
    /// geometry to work with).
    tiled: HashMap<usize, Rect>,
    /// Maximized pane id: when `Some`, that pane takes the whole workspace
    /// area and the split tree is hidden (but kept, so unzoom restores it).
    /// View state only: never persisted to the layout file.
    zoomed: Option<usize>,
    /// Nudges the UI thread whenever a pane delivers output.
    wake: Sender<()>,
}

impl Workspace {
    /// Boot workspace with a single pane (its reader gets a tokio thread).
    pub fn new(
        name: String,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &Sender<()>,
    ) -> Result<Self, String> {
        let pane = Pane::spawn(0, shell, scrollback, rt, wake)?;
        Ok(Self {
            name,
            panes: vec![pane],
            root: Some(Node::Pane(0)),
            focused: 0,
            next_id: 1,
            last_done: String::from("shell started"),
            activity: 0,
            seen: 0,
            rects: HashMap::new(),
            tiled: HashMap::new(),
            zoomed: None,
            wake: wake.clone(),
        })
    }

    /// Rebuild a workspace from a saved layout (fresh panes, each starting in
    /// its saved directory). `dirs` holds one directory per leaf in visual
    /// order; short/extra entries fall back to the process directory / are
    /// ignored, so old layout files without directories still load.
    pub fn from_layout(
        name: &str,
        layout: &TreeLayout,
        dirs: &[std::path::PathBuf],
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &Sender<()>,
    ) -> Result<Self, String> {
        let mut panes = Vec::new();
        let mut next_id = 0usize;
        let mut dir_idx = 0usize;
        let root = Node::from_layout(
            layout, &mut panes, &mut next_id, dirs, &mut dir_idx, shell, scrollback, rt, wake,
        )?;
        if panes.is_empty() {
            return Err("layout had no panes".into());
        }
        let focused = panes[0].id;
        Ok(Self {
            name: name.to_string(),
            panes,
            root: Some(root),
            focused,
            next_id,
            last_done: String::from("restored layout"),
            activity: 0,
            seen: 0,
            rects: HashMap::new(),
            tiled: HashMap::new(),
            zoomed: None,
            wake: wake.clone(),
        })
    }

    /// Persisted shape of this workspace's split tree.
    pub fn tree_layout(&self) -> TreeLayout {
        self.root
            .as_ref()
            .map(Node::tree_layout)
            .unwrap_or(TreeLayout::Pane)
    }

    /// Saved directories, one per leaf in visual order (same order that
    /// [`Workspace::from_layout`] consumes). Uses each pane's last-known cwd,
    /// falling back to the process directory for missing panes.
    pub fn leaf_cwds(&self) -> Vec<std::path::PathBuf> {
        let fallback = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        self.leaf_ids()
            .iter()
            .map(|id| {
                self.pane(*id)
                    .map(|p| p.cwd().to_path_buf())
                    .unwrap_or_else(|| fallback.clone())
            })
            .collect()
    }

    /// Borrow tree root.
    pub fn root(&self) -> Option<&Node> {
        self.root.as_ref()
    }

    /// Mutable pane by id.
    pub fn pane_mut(&mut self, id: usize) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == id)
    }

    /// Immutable pane by id.
    pub fn pane(&self, id: usize) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == id)
    }

    /// Kill every shell (test cleanup so shells never outlive the suite and
    /// pile up ConPTY pressure for later tests).
    #[cfg(test)]
    pub(crate) fn kill_all(&mut self) {
        for p in &mut self.panes {
            p.kill();
        }
    }

    /// Poll every pane; accumulate activity; note newly exited shells.
    /// Returns the ids of panes whose shell exited since the last poll.
    pub fn poll_panes(&mut self) -> Vec<usize> {
        let was_dead: Vec<usize> = self.panes.iter().filter(|p| p.dead).map(|p| p.id).collect();
        for p in &mut self.panes {
            self.activity += p.poll() as u64;
        }
        let mut died = Vec::new();
        for p in &self.panes {
            if p.dead && !was_dead.contains(&p.id) {
                self.last_done = format!("pane {}: shell exited", p.id);
                died.push(p.id);
            }
        }
        died
    }

    /// Leaf ids in visual order.
    pub fn leaf_ids(&self) -> Vec<usize> {
        let mut out = Vec::new();
        if let Some(r) = &self.root {
            r.leaves(&mut out);
        }
        out
    }

    /// (alive panes, total panes).
    pub fn alive(&self) -> (usize, usize) {
        let total = self.leaf_ids().len();
        let alive = self
            .leaf_ids()
            .iter()
            .filter(|id| self.pane(**id).is_some_and(|p| !p.dead))
            .count();
        (alive, total)
    }

    /// True when background output arrived since last acknowledged.
    pub fn unread(&self) -> bool {
        self.activity != self.seen
    }

    /// Acknowledge activity (called when workspace becomes current).
    pub fn mark_seen(&mut self) {
        self.seen = self.activity;
    }

    /// One-line inbox status: `2/2 alive · kalk 1+1 = 2`.
    pub fn status_line(&self) -> String {
        let (alive, total) = self.alive();
        let flag = if self.unread() { " •" } else { "" };
        format!("{alive}/{total} alive · {}{flag}", self.last_done)
    }

    /// Split focused leaf; returns new pane id (or error text). The new
    /// shell starts in the focused pane's directory.
    pub fn split(
        &mut self,
        dir: SplitDir,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
    ) -> Result<usize, String> {
        // A maximized pane hides the tree a split would extend: restore
        // first so the new pane is actually visible.
        self.zoomed = None;
        let id = self.next_id;
        self.next_id += 1;
        // The new shell starts where the focused one is (last known cwd;
        // spawn falls back to the process directory when it is gone).
        let cwd = self
            .pane(self.focused)
            .map(|p| p.cwd().to_path_buf())
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        log::debug!("split pane {id} in {}", cwd.display());
        let pane = Pane::spawn_in(id, shell, scrollback, rt, &self.wake.clone(), &cwd)?;
        self.panes.push(pane);
        let old = self.focused;
        let replacement = Node::Split {
            dir,
            ratio: 0.5,
            first: Box::new(Node::Pane(old)),
            second: Box::new(Node::Pane(id)),
        };
        if let Some(root) = &mut self.root {
            if old == root_single(root) {
                *root = replacement;
            } else {
                root.replace(old, replacement);
            }
        }
        self.focused = id;
        Ok(id)
    }

    /// Remove any leaf by id (used for delayed close-fx removal).
    /// Returns true when the workspace is now empty. Missing ids are ignored.
    /// Focus moves to the first surviving leaf when it pointed at `id`.
    pub fn remove_leaf(&mut self, id: usize) -> bool {
        if !self.leaf_ids().contains(&id) {
            return self.leaf_ids().is_empty();
        }
        // Never maximize a pane that no longer exists.
        if self.zoomed == Some(id) {
            self.zoomed = None;
        }
        Node::remove(&mut self.root, id);
        if Node::is_empty_opt(&self.root) {
            self.panes.clear();
            return true;
        }
        self.panes.retain(|p| p.id != id);
        if self.focused == id {
            self.focused = *self.leaf_ids().first().unwrap_or(&0);
        }
        false
    }

    /// Cycle focus; returns newly focused pane id.
    pub fn cycle(&mut self, dir: i32) -> usize {
        let ids = self.leaf_ids();
        if ids.is_empty() {
            return self.focused;
        }
        let pos = ids.iter().position(|&i| i == self.focused).unwrap_or(0);
        let next = if dir >= 0 {
            (pos + 1) % ids.len()
        } else {
            (pos + ids.len() - 1) % ids.len()
        };
        self.focused = ids[next];
        self.focused
    }

    /// Maximized pane id, if the workspace is zoomed (ui gives it the whole
    /// area instead of tiling the split tree).
    pub fn zoomed(&self) -> Option<usize> {
        self.zoomed
    }

    /// Toggle maximize on the focused pane. Zooming a lone pane is a no-op
    /// (there is nothing to hide); otherwise the focused pane takes all the
    /// space until toggled again or focus moves. Returns the new state.
    pub fn toggle_zoom(&mut self) -> Option<usize> {
        if self.zoomed.is_some() {
            self.zoomed = None;
            return None;
        }
        if self.leaf_ids().len() < 2 {
            return None;
        }
        self.zoomed = Some(self.focused);
        self.zoomed
    }

    /// Restore the tiled layout. Returns true when a zoom was active.
    pub fn unzoom(&mut self) -> bool {
        if self.zoomed.is_some() {
            self.zoomed = None;
            true
        } else {
            false
        }
    }

    /// Refresh cached pane rects (called by ui after every layout).
    pub fn set_rects(&mut self, rects: &[(usize, Rect)]) {
        self.rects.clear();
        self.rects.extend(rects.iter().copied());
    }

    /// Refresh cached fully-tiled rects (called by ui every frame alongside
    /// [`set_rects`], even while zoomed). Directional focus reads these when
    /// restoring a maximized pane, so one keypress both restores and moves.
    pub fn set_tiled_rects(&mut self, rects: &[(usize, Rect)]) {
        self.tiled.clear();
        self.tiled.extend(rects.iter().copied());
    }

    /// Cached draw rect of a pane, if it has been laid out.
    pub fn pane_rect(&self, id: usize) -> Option<Rect> {
        self.rects.get(&id).copied()
    }

    /// Pane whose rect contains the given cell, if any (mouse hit-testing).
    pub fn pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.rects
            .iter()
            .find(|(_, r)| {
                col >= r.x
                    && col < r.x + r.width
                    && row >= r.y
                    && row < r.y + r.height
            })
            .map(|(id, _)| *id)
    }

    /// Focus the nearest pane in `dir`. A focus key both restores a
    /// maximized pane and moves: geometry comes from the last tiled layout
    /// in that case, since the cached draw rects only hold the zoomed pane.
    /// Returns the new focus, or `None` when nothing lies that way
    /// (or rects are not known yet).
    pub fn focus_direction(&mut self, dir: Direction) -> Option<usize> {
        let from_zoom = self.unzoom();
        let map = if from_zoom { &self.tiled } else { &self.rects };
        let cur = *map.get(&self.focused)?;
        let (best, _) = self
            .leaf_ids()
            .into_iter()
            .filter(|&id| id != self.focused)
            .filter_map(|id| map.get(&id).map(|r| (id, *r)))
            .filter(|(_, r)| beyond(cur, *r, dir))
            .map(|(id, r)| (id, score(cur, r, dir)))
            .min_by(|a, b| a.1.cmp(&b.1))?;
        self.focused = best;
        Some(best)
    }

    /// Grow the focused pane toward `dir` by `delta` (0..1 of the split),
    /// adjusting the nearest ancestor split with matching orientation.
    /// Returns true when a split moved (caller saves the layout).
    pub fn resize_focused(&mut self, dir: Direction, delta: f32) -> bool {
        let Some(root) = &mut self.root else {
            return false;
        };
        resize_in(root, self.focused, dir, delta)
    }
}

/// Whether `r` lies strictly beyond `cur` toward `dir`.
fn beyond(cur: Rect, r: Rect, dir: Direction) -> bool {
    let (cx, cy, cw, ch) = (cur.x as i32, cur.y as i32, cur.width as i32, cur.height as i32);
    let (x, y, w, h) = (r.x as i32, r.y as i32, r.width as i32, r.height as i32);
    match dir {
        Direction::Left => x + w <= cx + 1,
        Direction::Right => x >= cx + cw - 1,
        Direction::Up => y + h <= cy + 1,
        Direction::Down => y >= cy + ch - 1,
    }
}

/// (primary gap, secondary center offset): smaller wins.
fn score(cur: Rect, r: Rect, dir: Direction) -> (i32, i32) {
    let (cx, cy, cw, ch) = (cur.x as i32, cur.y as i32, cur.width as i32, cur.height as i32);
    let (x, y, w, h) = (r.x as i32, r.y as i32, r.width as i32, r.height as i32);
    match dir {
        Direction::Left => (cx - (x + w), ((cy + ch / 2) - (y + h / 2)).abs()),
        Direction::Right => ((x - (cx + cw)), ((cy + ch / 2) - (y + h / 2)).abs()),
        Direction::Up => ((cy - (y + h)), ((cx + cw / 2) - (x + w / 2)).abs()),
        Direction::Down => ((y - (cy + ch)), ((cx + cw / 2) - (x + w / 2)).abs()),
    }
}

/// Whether `target` is a leaf of this subtree.
fn contains(node: &Node, target: usize) -> bool {
    match node {
        Node::Pane(id) => *id == target,
        Node::Split { first, second, .. } => contains(first, target) || contains(second, target),
    }
}

/// Move the deepest ancestor split with matching orientation, growing the
/// side that holds `target`. Left/Right match side-by-side (Vertical)
/// splits, Up/Down match stacked (Horizontal) ones.
fn resize_in(node: &mut Node, target: usize, dir: Direction, delta: f32) -> bool {
    let Node::Split {
        dir: split_dir,
        ratio,
        first,
        second,
    } = node
    else {
        return false;
    };
    if resize_in(first, target, dir, delta) || resize_in(second, target, dir, delta) {
        return true;
    }
    let vertical = matches!(split_dir, SplitDir::Vertical);
    if matches!(dir, Direction::Left | Direction::Right) != vertical {
        return false;
    }
    let in_first = contains(first, target);
    if !in_first && !contains(second, target) {
        return false;
    }
    let grow_first = match (dir, in_first) {
        (Direction::Right, true)
        | (Direction::Left, false)
        | (Direction::Down, true)
        | (Direction::Up, false) => true,
        _ => false,
    };
    *ratio = (*ratio + if grow_first { delta } else { -delta }).clamp(0.1, 0.9);
    true
}

/// Focused id when root is a single leaf, else `usize::MAX` sentinel.
fn root_single(root: &Node) -> usize {
    match root {
        Node::Pane(id) => *id,
        _ => usize::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(id: usize) -> Option<Node> {
        Some(Node::Pane(id))
    }

    #[test]
    fn leaves_in_visual_order() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Pane(0)),
            second: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Pane(1)),
                second: Box::new(Node::Pane(2)),
            }),
        };
        let mut out = Vec::new();
        tree.leaves(&mut out);
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn replace_swaps_leaf_for_split() {
        let mut tree = Node::Pane(0);
        let replacement = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Pane(0)),
            second: Box::new(Node::Pane(1)),
        };
        assert!(tree.replace(0, replacement));
        let mut out = Vec::new();
        tree.leaves(&mut out);
        assert_eq!(out, vec![0, 1]);
    }

    #[test]
    fn remove_collapses_parent_split() {
        let mut root = Some(Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: Box::new(Node::Pane(0)),
            second: Box::new(Node::Pane(1)),
        });
        assert!(Node::remove(&mut root, 1));
        // Surviving leaf 0 takes the split's place.
        let mut out = Vec::new();
        root.as_ref().unwrap().leaves(&mut out);
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn remove_last_leaf_empties_root() {
        let mut root = leaf(7);
        assert!(Node::remove(&mut root, 7));
        assert!(Node::is_empty_opt(&root));
    }

    #[test]
    fn remove_missing_leaf_keeps_tree() {
        let mut root = Some(Node::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Pane(0)),
            second: Box::new(Node::Pane(1)),
        });
        assert!(!Node::remove(&mut root, 99));
        let mut out = Vec::new();
        root.as_ref().unwrap().leaves(&mut out);
        assert_eq!(out, vec![0, 1]);
    }

    /// Geometry: left pane focuses right and back; resize moves the ratio.
    #[test]
    fn directional_focus_and_resize() {
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut ws = Workspace {
            name: "t".into(),
            panes: Vec::new(),
            root: Some(Node::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Pane(0)),
                second: Box::new(Node::Pane(1)),
            }),
            focused: 0,
            next_id: 2,
            last_done: String::new(),
            activity: 0,
            seen: 0,
            rects: HashMap::from([
                (0, Rect::new(0, 0, 40, 20)),
                (1, Rect::new(40, 0, 40, 20)),
            ]),
            tiled: HashMap::from([
                (0, Rect::new(0, 0, 40, 20)),
                (1, Rect::new(40, 0, 40, 20)),
            ]),
            zoomed: None,
            wake,
        };
        assert_eq!(ws.focus_direction(Direction::Right), Some(1));
        assert_eq!(ws.focus_direction(Direction::Left), Some(0));
        assert_eq!(ws.focus_direction(Direction::Up), None);
        assert!(ws.resize_focused(Direction::Right, 0.1));
        let Node::Split { ratio, .. } = ws.root.as_ref().unwrap() else {
            panic!("root stays a split");
        };
        assert!((ratio - 0.6).abs() < 1e-6);
        // Up/Down match no side-by-side split.
        assert!(!ws.resize_focused(Direction::Up, 0.1));
    }

    /// Zoom toggles maximize on the focused pane; unzoom reports whether it
    /// cleared anything; removing the zoomed pane clears zoom and refocuses.
    #[test]
    fn zoom_toggle_unzoom_and_guards() {
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut ws = Workspace {
            name: "t".into(),
            panes: Vec::new(),
            root: Some(Node::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Pane(0)),
                second: Box::new(Node::Pane(1)),
            }),
            focused: 0,
            next_id: 2,
            last_done: String::new(),
            activity: 0,
            seen: 0,
            rects: HashMap::new(),
            tiled: HashMap::new(),
            zoomed: None,
            wake: wake.clone(),
        };
        assert_eq!(ws.zoomed(), None);
        // Toggle maximizes the focused pane, toggle again restores.
        assert_eq!(ws.toggle_zoom(), Some(0));
        assert_eq!(ws.zoomed(), Some(0));
        assert_eq!(ws.toggle_zoom(), None);
        assert_eq!(ws.zoomed(), None);
        // unzoom reports whether it cleared an active zoom.
        assert!(!ws.unzoom());
        ws.toggle_zoom();
        assert!(ws.unzoom());
        assert!(!ws.unzoom());
        // Removing the zoomed pane clears zoom and moves focus to a survivor.
        ws.toggle_zoom();
        assert!(!ws.remove_leaf(0));
        assert_eq!(ws.zoomed(), None);
        assert_eq!(ws.focused, 1);
        assert_eq!(ws.leaf_ids(), vec![1]);
        // A lone pane cannot zoom: nothing to hide.
        let mut solo = Workspace {
            name: "s".into(),
            panes: Vec::new(),
            root: Some(Node::Pane(0)),
            focused: 0,
            next_id: 1,
            last_done: String::new(),
            activity: 0,
            seen: 0,
            rects: HashMap::new(),
            tiled: HashMap::new(),
            zoomed: None,
            wake,
        };
        assert_eq!(solo.toggle_zoom(), None);
        assert_eq!(solo.zoomed(), None);
        assert!(!solo.unzoom());
    }

    /// Directional focus while zoomed restores and moves in one step, using
    /// the tiled rects (the draw rects only hold the maximized pane).
    #[test]
    fn zoomed_focus_direction_restores_and_moves() {
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let tiled = HashMap::from([
            (0, Rect::new(0, 0, 40, 20)),
            (1, Rect::new(40, 0, 40, 20)),
        ]);
        let mut ws = Workspace {
            name: "t".into(),
            panes: Vec::new(),
            root: Some(Node::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Pane(0)),
                second: Box::new(Node::Pane(1)),
            }),
            focused: 0,
            next_id: 2,
            last_done: String::new(),
            activity: 0,
            seen: 0,
            // Draw rects hold only the maximized pane, as ui leaves them.
            rects: HashMap::from([(0, Rect::new(0, 0, 80, 20))]),
            tiled,
            zoomed: Some(0),
            wake,
        };
        // Right: restores and lands on pane 1.
        assert_eq!(ws.focus_direction(Direction::Right), Some(1));
        assert_eq!(ws.zoomed(), None);
        assert_eq!(ws.focused, 1);
        // Left with nothing that way: still restores, focus stays.
        ws.toggle_zoom();
        assert_eq!(ws.focus_direction(Direction::Right), None);
        assert_eq!(ws.zoomed(), None);
        assert_eq!(ws.focused, 1);
        // No tiled geometry known: restores, cannot move.
        ws.toggle_zoom();
        ws.tiled.clear();
        assert_eq!(ws.focus_direction(Direction::Left), None);
        assert_eq!(ws.zoomed(), None);
    }

    /// Mouse hit-testing maps a cell to the pane whose rect contains it.
    #[test]
    fn pane_at_hit_tests_rects() {
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut ws = Workspace {
            name: "t".into(),
            panes: Vec::new(),
            root: None,
            focused: 0,
            next_id: 0,
            last_done: String::new(),
            activity: 0,
            seen: 0,
            rects: HashMap::from([
                (0, Rect::new(0, 0, 40, 20)),
                (1, Rect::new(40, 0, 40, 20)),
            ]),
            tiled: HashMap::from([
                (0, Rect::new(0, 0, 40, 20)),
                (1, Rect::new(40, 0, 40, 20)),
            ]),
            zoomed: None,
            wake,
        };
        assert_eq!(ws.pane_at(10, 5), Some(0));
        assert_eq!(ws.pane_at(45, 5), Some(1));
        assert_eq!(ws.pane_at(90, 5), None);
        assert_eq!(ws.pane_at(10, 25), None);
        ws.rects.clear();
        assert_eq!(ws.pane_at(0, 0), None);
    }
}
