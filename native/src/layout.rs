use ddg_agents_proto::host::{MAX_SPLIT_RATIO_BP, MIN_SPLIT_RATIO_BP, PaneNode, SplitAxis, TabId};

pub const PANE_DIVIDER: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitSide {
    First,
    Second,
}

pub type SplitPath = Vec<SplitSide>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavDir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DividerHit {
    pub path: SplitPath,
    pub axis: SplitAxis,
    pub rect: PaneRect,
    pub parent_origin: f32,
    pub parent_extent: f32,
}

pub fn compute_layout(
    node: &PaneNode,
    bounds: PaneRect,
    divider_px: f32,
) -> (Vec<(TabId, PaneRect)>, Vec<DividerHit>) {
    fn visit(
        node: &PaneNode,
        bounds: PaneRect,
        divider: f32,
        path: &mut SplitPath,
        panes: &mut Vec<(TabId, PaneRect)>,
        dividers: &mut Vec<DividerHit>,
    ) {
        match node {
            PaneNode::Leaf { tab_id } => panes.push((tab_id.clone(), bounds)),
            PaneNode::Split {
                axis,
                ratio_bp,
                first,
                second,
            } => {
                let horizontal = *axis == SplitAxis::Horizontal;
                let (origin, extent) = if horizontal {
                    (bounds.x, bounds.w)
                } else {
                    (bounds.y, bounds.h)
                };
                let avail = (extent - divider).max(0.0);
                let first_extent = (avail * (f32::from(*ratio_bp) / 10_000.0)).round();
                let along_axis = |origin, extent| {
                    if horizontal {
                        PaneRect {
                            x: origin,
                            w: extent,
                            ..bounds
                        }
                    } else {
                        PaneRect {
                            y: origin,
                            h: extent,
                            ..bounds
                        }
                    }
                };
                dividers.push(DividerHit {
                    path: path.clone(),
                    axis: *axis,
                    rect: along_axis(origin + first_extent, divider),
                    parent_origin: origin,
                    parent_extent: avail,
                });
                path.push(SplitSide::First);
                visit(
                    first,
                    along_axis(origin, first_extent),
                    divider,
                    path,
                    panes,
                    dividers,
                );
                *path.last_mut().expect("pushed split side") = SplitSide::Second;
                visit(
                    second,
                    along_axis(
                        origin + first_extent + divider,
                        (avail - first_extent).max(0.0),
                    ),
                    divider,
                    path,
                    panes,
                    dividers,
                );
                path.pop();
            }
        }
    }

    let mut panes = Vec::new();
    let mut dividers = Vec::new();
    visit(
        node,
        bounds,
        divider_px,
        &mut Vec::new(),
        &mut panes,
        &mut dividers,
    );
    (panes, dividers)
}

pub fn pane_grid(rect: PaneRect, font_w: f32, font_h: f32) -> (u16, u16) {
    (
        ((rect.w - 2.0) / font_w).floor().clamp(1.0, 500.0) as u16,
        ((rect.h - 2.0) / font_h).floor().clamp(1.0, 300.0) as u16,
    )
}

pub fn ratio_bp_from_cursor(cursor: f32, parent_origin: f32, parent_extent: f32) -> u16 {
    (((cursor - parent_origin) / parent_extent.max(1.0)) * 10_000.0)
        .round()
        .clamp(f32::from(MIN_SPLIT_RATIO_BP), f32::from(MAX_SPLIT_RATIO_BP)) as u16
}

pub fn set_ratio_at(node: &mut PaneNode, path: &[SplitSide], ratio_bp: u16) -> bool {
    let PaneNode::Split {
        first,
        second,
        ratio_bp: ratio,
        ..
    } = node
    else {
        return false;
    };
    match path.split_first() {
        None => {
            *ratio = ratio_bp.clamp(MIN_SPLIT_RATIO_BP, MAX_SPLIT_RATIO_BP);
            true
        }
        Some((side, rest)) => set_ratio_at(
            match side {
                SplitSide::First => first,
                SplitSide::Second => second,
            },
            rest,
            ratio_bp,
        ),
    }
}

pub fn ratio_at(node: &PaneNode, path: &[SplitSide]) -> Option<u16> {
    let PaneNode::Split {
        first,
        second,
        ratio_bp,
        ..
    } = node
    else {
        return None;
    };
    match path.split_first() {
        None => Some(*ratio_bp),
        Some((SplitSide::First, rest)) => ratio_at(first, rest),
        Some((SplitSide::Second, rest)) => ratio_at(second, rest),
    }
}

pub fn navigate_direction(
    panes: &[(TabId, PaneRect)],
    focused: &str,
    dir: NavDir,
) -> Option<TabId> {
    let (_, focus) = panes.iter().find(|(id, _)| id == focused)?;
    let axes = |rect: &PaneRect| match dir {
        NavDir::Left => (-rect.x - rect.w, rect.w, rect.y, rect.h),
        NavDir::Right => (rect.x, rect.w, rect.y, rect.h),
        NavDir::Up => (-rect.y - rect.h, rect.h, rect.x, rect.w),
        NavDir::Down => (rect.y, rect.h, rect.x, rect.w),
    };
    let (start, size, cross, cross_size) = axes(focus);
    panes
        .iter()
        .filter_map(|(id, rect)| {
            let (other, _, other_cross, other_size) = axes(rect);
            let gap = other - (start + size);
            if id == focused || gap < 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
                return None;
            }
            let overlap =
                (cross + cross_size).min(other_cross + other_size) - cross.max(other_cross);
            let center_distance =
                ((cross + cross_size / 2.0) - (other_cross + other_size / 2.0)).abs();
            Some((overlap <= 0.0, gap, center_distance, id))
        })
        .min_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.total_cmp(&b.1))
                .then(a.2.total_cmp(&b.2))
        })
        .map(|(_, _, _, id)| id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> PaneRect {
        PaneRect { x, y, w, h }
    }

    fn split(axis: SplitAxis, ratio_bp: u16, first: PaneNode, second: PaneNode) -> PaneNode {
        PaneNode::Split {
            axis,
            ratio_bp,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    fn leaf(id: &str) -> PaneNode {
        PaneNode::leaf(id.into())
    }

    #[test]
    fn nested_layout_accounts_for_origins_rounding_and_dividers() {
        let tree = split(
            SplitAxis::Horizontal,
            3500,
            leaf("a"),
            split(SplitAxis::Vertical, 6000, leaf("b"), leaf("c")),
        );
        let (panes, hits) = compute_layout(&tree, rect(10., 20., 205., 104.), PANE_DIVIDER);
        assert_eq!(
            panes,
            [
                ("a".into(), rect(10., 20., 70., 104.)),
                ("b".into(), rect(84., 20., 131., 60.)),
                ("c".into(), rect(84., 84., 131., 40.))
            ]
        );
        assert_eq!(
            hits,
            [
                DividerHit {
                    path: vec![],
                    axis: SplitAxis::Horizontal,
                    rect: rect(80., 20., 4., 104.),
                    parent_origin: 10.,
                    parent_extent: 201.
                },
                DividerHit {
                    path: vec![SplitSide::Second],
                    axis: SplitAxis::Vertical,
                    rect: rect(84., 80., 131., 4.),
                    parent_origin: 20.,
                    parent_extent: 100.
                },
            ]
        );
        let (panes, hits) = compute_layout(&tree, rect(0., 0., 1., 2.), PANE_DIVIDER);
        assert!(panes.iter().all(|(_, r)| r.w == 0.0));
        assert_eq!(hits[1].parent_extent, 0.0);
        assert_eq!(
            compute_layout(&leaf("a"), rect(2., 3., 4., 5.), PANE_DIVIDER),
            (vec![("a".into(), rect(2., 3., 4., 5.))], vec![])
        );
    }

    #[test]
    fn grid_uses_each_pane_size_and_caps_terminal_dimensions() {
        assert_eq!(pane_grid(rect(0., 0., 83., 45.), 8., 14.), (10, 3));
        assert_eq!(pane_grid(rect(0., 0., 0., 1.), 8., 14.), (1, 1));
        assert_eq!(pane_grid(rect(0., 0., 9000., 9000.), 8., 14.), (500, 300));
    }

    #[test]
    fn drag_ratio_clamps_and_only_updates_target_split() {
        assert_eq!(ratio_bp_from_cursor(40., 30., 40.), 2500);
        assert_eq!(ratio_bp_from_cursor(20., 30., 40.), 1500);
        assert_eq!(ratio_bp_from_cursor(80., 30., 40.), 8500);
        assert_eq!(ratio_bp_from_cursor(30.5, 30., 0.), 5000);
        assert_eq!(ratio_bp_from_cursor(31., 30., 3.), 3333);
        let mut tree = split(
            SplitAxis::Vertical,
            4000,
            split(SplitAxis::Horizontal, 5000, leaf("a"), leaf("b")),
            leaf("c"),
        );
        assert!(set_ratio_at(&mut tree, &[SplitSide::First], 9000));
        assert_eq!(ratio_at(&tree, &[SplitSide::First]), Some(8500));
        assert_eq!(ratio_at(&tree, &[]), Some(4000));
        assert!(set_ratio_at(&mut tree, &[], 0));
        assert_eq!(ratio_at(&tree, &[]), Some(1500));
        assert!(!set_ratio_at(&mut tree, &[SplitSide::Second], 5000));
        assert!(!set_ratio_at(
            &mut tree,
            &[SplitSide::First, SplitSide::Second, SplitSide::Second],
            5000
        ));
        assert_eq!(ratio_at(&tree, &[SplitSide::First]), Some(8500));
    }

    #[test]
    fn navigate_two_three_and_four_panes() {
        use NavDir::*;
        let bounds = rect(0., 0., 200., 160.);
        let two = split(SplitAxis::Horizontal, 5000, leaf("a"), leaf("b"));
        let three = split(
            SplitAxis::Horizontal,
            5000,
            leaf("a"),
            split(SplitAxis::Vertical, 3000, leaf("b"), leaf("c")),
        );
        let four = split(
            SplitAxis::Vertical,
            5000,
            split(SplitAxis::Horizontal, 5000, leaf("a"), leaf("b")),
            split(SplitAxis::Horizontal, 5000, leaf("c"), leaf("d")),
        );
        for (tree, from, dir, expected) in [
            (&two, "a", Right, Some("b")),
            (&two, "b", Left, Some("a")),
            (&two, "b", Down, None),
            (&two, "missing", Right, None),
            (&three, "a", Right, Some("c")),
            (&three, "b", Down, Some("c")),
            (&three, "c", Left, Some("a")),
            (&three, "c", Up, Some("b")),
            (&four, "a", Right, Some("b")),
            (&four, "b", Down, Some("d")),
            (&four, "d", Left, Some("c")),
            (&four, "c", Up, Some("a")),
            (&four, "a", Up, None),
            (&four, "c", Left, None),
        ] {
            let (panes, _) = compute_layout(tree, bounds, PANE_DIVIDER);
            assert_eq!(
                navigate_direction(&panes, from, dir).as_deref(),
                expected,
                "{from} {dir:?}"
            );
        }
    }

    #[test]
    fn navigation_prefers_overlap_then_uses_diagonal_fallback() {
        let panes = [
            ("a".into(), rect(0., 0., 30., 30.)),
            ("b".into(), rect(32., 32., 30., 30.)),
            ("c".into(), rect(65., 0., 30., 30.)),
        ];
        assert_eq!(
            navigate_direction(&panes, "a", NavDir::Right).as_deref(),
            Some("c")
        );
        assert_eq!(
            navigate_direction(&panes[..2], "a", NavDir::Right).as_deref(),
            Some("b")
        );
        assert_eq!(
            navigate_direction(&panes, "c", NavDir::Down).as_deref(),
            Some("b")
        );
    }
}
