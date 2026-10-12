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
            focus_seq: 1,
            last_seen: HashMap::from([(0, 1)]),
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
            focus_seq: 1,
            last_seen: HashMap::from([(0, 1)]),
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
            focus_seq: 1,
            last_seen: HashMap::from([(0, 1)]),
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
            focus_seq: 1,
            last_seen: HashMap::from([(0, 1)]),
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
            focus_seq: 1,
            last_seen: HashMap::from([(0, 1)]),
            wake,
        };
        assert_eq!(ws.pane_at(10, 5), Some(0));
        assert_eq!(ws.pane_at(45, 5), Some(1));
        assert_eq!(ws.pane_at(90, 5), None);
        assert_eq!(ws.pane_at(10, 25), None);
        ws.rects.clear();
        assert_eq!(ws.pane_at(0, 0), None);
    }

    /// Nesting depth counts `Split` levels iteratively (no recursion, so the
    /// validator itself cannot overflow on a hostile layout file).
    #[test]
    fn layout_depth_counts_nesting() {
        use crate::layout::TreeLayout;

        let mut tree = TreeLayout::Pane;
        assert_eq!(super::layout_depth(&tree), 0);
        for expected in 1..=(super::MAX_LAYOUT_DEPTH + 8) {
            tree = TreeLayout::split(SplitDir::Vertical, 0.5, tree, TreeLayout::Pane);
            assert_eq!(super::layout_depth(&tree), expected);
        }
    }
