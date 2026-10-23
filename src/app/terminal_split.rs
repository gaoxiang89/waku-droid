//! The split tree behind one bottom-panel terminal tab. A tab's terminals
//! fill the panel together — a single pane at first, then a flexbox tree of
//! panes and axes as the user splits. All layout arithmetic lives here so
//! the renderer and the pane-size feed the terminals resize from stay the
//! same numbers.

use gpui::{Axis, Entity, EntityId};

use crate::terminal::TerminalView;

/// Panes one tab may show at once. Beyond this a panel-height strip becomes
/// unreadable, so splitting stops (T3 Code draws the same line).
pub(crate) const MAX_SPLIT_PANES: usize = 4;
/// Flex-none footprint of one divider between two panes.
pub(crate) const SPLIT_DIVIDER_SIZE: f32 = 5.0;
/// Smallest pane a divider drag may leave behind, in pixels.
pub(crate) const SPLIT_MIN_PANE: f32 = 80.0;

/// Distribute `along_total` across `flexes` the way the flexbox will: one
/// divider per seam is set aside, the rest splits by grow weight. Also
/// returns the pixels one unit of flex is worth, which divider drags convert
/// pointer deltas with.
pub(crate) fn axis_child_sizes(along_total: f32, flexes: &[f32]) -> (Vec<f32>, f32) {
    let count = flexes.len();
    if count == 0 {
        return (Vec::new(), 0.0);
    }
    let dividers = SPLIT_DIVIDER_SIZE * (count.saturating_sub(1)) as f32;
    let available = (along_total - dividers).max(0.0);
    let total_flex: f32 = flexes.iter().sum();
    let total_flex = if total_flex <= f32::EPSILON {
        count as f32
    } else {
        total_flex
    };
    let sizes = flexes
        .iter()
        .map(|weight| available * weight.max(0.0) / total_flex)
        .collect();
    (sizes, available / total_flex)
}

pub(crate) enum SplitMember {
    Pane(Entity<TerminalView>),
    Axis(SplitAxis),
}

pub(crate) struct SplitAxis {
    /// Direction the children stack along: `Horizontal` is side by side,
    /// `Vertical` is stacked.
    pub(crate) axis: Axis,
    pub(crate) members: Vec<SplitMember>,
    /// One grow weight per member; the divider drag transfers weight between
    /// neighbors, so only their ratio matters.
    pub(crate) flexes: Vec<f32>,
}

/// One tab's worth of terminals: the split tree plus the pane that tab
/// activation, the tab label, and the strip's split button act on. Keyboard
/// splits and closes do not use it — those arrive along the focus path from
/// the focused pane itself.
pub(crate) struct SplitGroup {
    pub(crate) root: SplitMember,
    pub(crate) active_pane: Option<EntityId>,
}

/// Pixel box of one pane, produced by mirroring the flexbox layout the
/// renderer builds. Kept for the tests that pin the mirror to the flexbox
/// arithmetic; the renderer computes the same numbers inline because divider
/// drags need each axis's child sizes as they walk the tree.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PaneSize {
    pub(crate) width: f32,
    pub(crate) height: f32,
}

/// State of a divider drag, held by `Waku` between the divider's mouse-down
/// and the next left mouse-up anywhere.
#[derive(Clone)]
pub(crate) struct SplitDividerDrag {
    pub(crate) group_index: usize,
    /// Path of member indexes from the root to the axis owning the divider;
    /// empty addresses the root when the root is an axis.
    pub(crate) path: Vec<usize>,
    /// The divider sits between `members[divider_after]` and its successor.
    pub(crate) divider_after: usize,
    pub(crate) axis: Axis,
    pub(crate) start_mouse_x: f32,
    pub(crate) start_mouse_y: f32,
    start_first_size: f32,
    start_second_size: f32,
    space_per_flex: f32,
}

impl SplitDividerDrag {
    /// `first_size`/`second_size` are the two neighbors' sizes in pixels at
    /// drag start, from the same mirrored arithmetic that sizes their grids.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        group_index: usize,
        path: Vec<usize>,
        divider_after: usize,
        axis: Axis,
        start_mouse_x: f32,
        start_mouse_y: f32,
        first_size: f32,
        second_size: f32,
        space_per_flex: f32,
    ) -> Self {
        Self {
            group_index,
            path,
            divider_after,
            axis,
            start_mouse_x,
            start_mouse_y,
            start_first_size: first_size,
            start_second_size: second_size,
            space_per_flex,
        }
    }

    /// The flexes the dragged pair should hold for the pointer's current
    /// position, clamped so neither pane shrinks under `SPLIT_MIN_PANE`.
    pub(crate) fn flexes_for(&self, mouse_x: f32, mouse_y: f32) -> (f32, f32) {
        let delta = match self.axis {
            Axis::Horizontal => mouse_x - self.start_mouse_x,
            Axis::Vertical => mouse_y - self.start_mouse_y,
        };
        let pair_total = self.start_first_size + self.start_second_size;
        let first = (self.start_first_size + delta).clamp(
            SPLIT_MIN_PANE,
            (pair_total - SPLIT_MIN_PANE).max(SPLIT_MIN_PANE),
        );
        let second = (pair_total - first).max(0.0);
        if self.space_per_flex <= f32::EPSILON {
            return (1.0, 1.0);
        }
        (first / self.space_per_flex, second / self.space_per_flex)
    }
}

impl SplitMember {
    fn pane_count(&self) -> usize {
        match self {
            SplitMember::Pane(_) => 1,
            SplitMember::Axis(axis) => axis.members.iter().map(SplitMember::pane_count).sum(),
        }
    }

    fn panes(&self) -> Vec<Entity<TerminalView>> {
        match self {
            SplitMember::Pane(terminal) => vec![terminal.clone()],
            SplitMember::Axis(axis) => axis.members.iter().flat_map(SplitMember::panes).collect(),
        }
    }

    fn contains(&self, id: EntityId) -> bool {
        match self {
            SplitMember::Pane(terminal) => terminal.entity_id() == id,
            SplitMember::Axis(axis) => axis.members.iter().any(|member| member.contains(id)),
        }
    }

    fn first_pane(&self) -> Option<Entity<TerminalView>> {
        match self {
            SplitMember::Pane(terminal) => Some(terminal.clone()),
            SplitMember::Axis(axis) => axis.members.iter().find_map(SplitMember::first_pane),
        }
    }

    /// Split the pane `target` in two, placing `new_pane` after it along
    /// `axis`. A leaf becomes an axis; a matching-axis parent absorbs the new
    /// pane directly; a perpendicular parent nests, so a split right inside a
    /// stacked pair yields the expected grid.
    fn split_pane(&mut self, target: EntityId, new_pane: Entity<TerminalView>, axis: Axis) {
        match self {
            SplitMember::Pane(terminal) if terminal.entity_id() == target => {
                let old = std::mem::replace(
                    self,
                    SplitMember::Axis(SplitAxis {
                        axis,
                        members: Vec::new(),
                        flexes: Vec::new(),
                    }),
                );
                let SplitMember::Axis(new_axis) = self else {
                    unreachable!("just replaced with an axis");
                };
                new_axis.members = vec![old, SplitMember::Pane(new_pane)];
                new_axis.flexes = vec![1.0, 1.0];
            }
            SplitMember::Axis(parent) => {
                let Some(index) = parent
                    .members
                    .iter()
                    .position(|member| member.contains(target))
                else {
                    return;
                };
                if parent.axis == axis
                    && matches!(&parent.members[index], SplitMember::Pane(terminal) if terminal.entity_id() == target)
                {
                    parent
                        .members
                        .insert(index + 1, SplitMember::Pane(new_pane));
                    parent.flexes.insert(index + 1, 1.0);
                } else {
                    parent.members[index].split_pane(target, new_pane, axis);
                }
            }
            SplitMember::Pane(_) => {}
        }
    }

    /// Remove the pane `target` from below this member, collapsing axes the
    /// removal pared down to a single child. Returns whether the target was
    /// found; a root that is itself the target is left in place for the
    /// group to detect.
    fn remove_pane(&mut self, target: EntityId) -> bool {
        match self {
            SplitMember::Pane(terminal) => terminal.entity_id() == target,
            SplitMember::Axis(axis) => {
                for index in 0..axis.members.len() {
                    let leaf_target = matches!(
                        &axis.members[index],
                        SplitMember::Pane(terminal) if terminal.entity_id() == target
                    );
                    if leaf_target {
                        axis.members.remove(index);
                        axis.flexes.remove(index);
                    } else if axis.members[index].remove_pane(target) {
                        // A child axis that lost a pane may hold one member;
                        // hoist it so the tree never carries dead axes.
                        if let SplitMember::Axis(child) = &mut axis.members[index]
                            && child.members.len() == 1
                        {
                            let only = child.members.pop().unwrap();
                            axis.members[index] = only;
                        }
                    } else {
                        continue;
                    }
                    if axis.members.len() == 1 {
                        let only = axis.members.pop().unwrap();
                        *self = only;
                    }
                    return true;
                }
                false
            }
        }
    }

    /// The axis at `path`, addressing children by index from the root.
    fn axis_at_mut(&mut self, path: &[usize]) -> Option<&mut SplitAxis> {
        let mut member = self;
        for index in path {
            let SplitMember::Axis(axis) = member else {
                return None;
            };
            member = axis.members.get_mut(*index)?;
        }
        match member {
            SplitMember::Axis(axis) => Some(axis),
            SplitMember::Pane(_) => None,
        }
    }

    /// Mirror the flexbox layout: distribute `width`/`height` across the
    /// tree exactly as `flex_grow(weight).flex_basis(0)` children plus
    /// flex-none dividers will be laid out.
    #[cfg(test)]
    fn pane_sizes(&self, width: f32, height: f32, out: &mut Vec<(EntityId, PaneSize)>) {
        match self {
            SplitMember::Pane(terminal) => {
                out.push((terminal.entity_id(), PaneSize { width, height }))
            }
            SplitMember::Axis(axis) => {
                if axis.members.is_empty() {
                    return;
                }
                let along = match axis.axis {
                    Axis::Horizontal => width,
                    Axis::Vertical => height,
                };
                let (sizes, _) = axis_child_sizes(along, &axis.flexes);
                for (index, member) in axis.members.iter().enumerate() {
                    let child_along = sizes.get(index).copied().unwrap_or(0.0);
                    let (child_width, child_height) = match axis.axis {
                        Axis::Horizontal => (child_along, height),
                        Axis::Vertical => (width, child_along),
                    };
                    member.pane_sizes(child_width, child_height, out);
                }
            }
        }
    }
}

impl SplitGroup {
    pub(crate) fn single(terminal: Entity<TerminalView>) -> Self {
        Self {
            root: SplitMember::Pane(terminal),
            active_pane: None,
        }
    }

    pub(crate) fn pane_count(&self) -> usize {
        self.root.pane_count()
    }

    pub(crate) fn panes(&self) -> Vec<Entity<TerminalView>> {
        self.root.panes()
    }

    pub(crate) fn contains(&self, id: EntityId) -> bool {
        self.root.contains(id)
    }

    /// The pane tab activation and the strip's split button target: the
    /// tracked pane while it lives in the tree, otherwise the first pane.
    pub(crate) fn active_or_first(&self) -> Option<Entity<TerminalView>> {
        if let Some(active) = self.active_pane
            && self.root.contains(active)
        {
            return self
                .panes()
                .into_iter()
                .find(|terminal| terminal.entity_id() == active);
        }
        self.root.first_pane()
    }

    pub(crate) fn set_active(&mut self, id: EntityId) {
        if self.root.contains(id) {
            self.active_pane = Some(id);
        }
    }

    /// Split `pane` in two along `axis`, adding `new_pane` after it. The new
    /// pane becomes the tracked one; nothing happens past the pane cap.
    pub(crate) fn split_pane(
        &mut self,
        pane: EntityId,
        new_pane: Entity<TerminalView>,
        axis: Axis,
    ) -> bool {
        if self.pane_count() >= MAX_SPLIT_PANES || !self.root.contains(pane) {
            return false;
        }
        let new_pane_id = new_pane.entity_id();
        self.root.split_pane(pane, new_pane, axis);
        self.active_pane = Some(new_pane_id);
        true
    }

    /// Remove `pane`. Returns false when the group emptied, in which case the
    /// caller drops the whole tab; the tree itself is emptied too so queries
    /// cannot see the removed pane.
    pub(crate) fn remove_pane(&mut self, pane: EntityId) -> bool {
        if !self.root.remove_pane(pane) {
            return true;
        }
        let emptied = matches!(&self.root, SplitMember::Pane(terminal) if terminal.entity_id() == pane)
            || matches!(&self.root, SplitMember::Axis(axis) if axis.members.is_empty());
        if emptied {
            self.active_pane = None;
            self.root = SplitMember::Axis(SplitAxis {
                axis: Axis::Horizontal,
                members: Vec::new(),
                flexes: Vec::new(),
            });
            return false;
        }
        if let Some(first) = self.root.first_pane() {
            self.set_active(first.entity_id());
        }
        true
    }

    /// Write the dragged divider's flexes. Returns false when the group or
    /// axis it addressed is gone.
    pub(crate) fn resize_divider(
        &mut self,
        drag: &SplitDividerDrag,
        mouse_x: f32,
        mouse_y: f32,
    ) -> bool {
        let Some(axis) = self.root.axis_at_mut(&drag.path) else {
            return false;
        };
        let (first, second) = drag.flexes_for(mouse_x, mouse_y);
        let first_index = drag.divider_after;
        if axis.flexes.len() < first_index + 2 {
            return false;
        }
        axis.flexes[first_index] = first;
        axis.flexes[first_index + 1] = second;
        true
    }

    #[cfg(test)]
    pub(crate) fn pane_sizes(&self, width: f32, height: f32) -> Vec<(EntityId, PaneSize)> {
        let mut sizes = Vec::new();
        self.root.pane_sizes(width, height, &mut sizes);
        sizes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalView;

    fn group_with(
        cx: &mut gpui::TestAppContext,
        count: usize,
    ) -> (Vec<Entity<TerminalView>>, SplitGroup) {
        let terminals: Vec<_> = (0..count)
            .map(|_| cx.update(|cx| TerminalView::inert_test_view_entity(cx)))
            .collect();
        let mut group = SplitGroup::single(terminals[0].clone());
        group.set_active(terminals[0].entity_id());
        for terminal in &terminals[1..] {
            let anchor = group.active_or_first().unwrap();
            assert!(group.split_pane(anchor.entity_id(), terminal.clone(), Axis::Horizontal));
            group.set_active(terminal.entity_id());
        }
        (terminals, group)
    }

    #[gpui::test]
    fn split_wraps_a_single_pane_in_an_axis(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 1);
        let new = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        assert!(group.split_pane(terminals[0].entity_id(), new.clone(), Axis::Horizontal));
        assert_eq!(group.pane_count(), 2);
        assert!(group.contains(new.entity_id()));
        assert_eq!(
            group.active_or_first().unwrap().entity_id(),
            new.entity_id()
        );
    }

    #[gpui::test]
    fn same_axis_splits_flatten_instead_of_nesting(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 1);
        let second = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(terminals[0].entity_id(), second.clone(), Axis::Horizontal);
        let third = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(second.entity_id(), third, Axis::Horizontal);

        let SplitMember::Axis(axis) = &group.root else {
            panic!("root should be an axis");
        };
        assert_eq!(axis.axis, Axis::Horizontal);
        assert_eq!(axis.members.len(), 3);
        assert_eq!(axis.flexes.len(), 3);
        assert_eq!(group.pane_count(), 3);
    }

    #[gpui::test]
    fn perpendicular_split_nests_inside_its_parent(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 1);
        let second = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(terminals[0].entity_id(), second, Axis::Horizontal);
        let third = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(terminals[0].entity_id(), third, Axis::Vertical);

        let SplitMember::Axis(root) = &group.root else {
            panic!("root should be an axis");
        };
        assert_eq!(root.axis, Axis::Horizontal);
        let SplitMember::Axis(nested) = &root.members[0] else {
            panic!("first child should be a nested axis");
        };
        assert_eq!(nested.axis, Axis::Vertical);
        assert_eq!(nested.members.len(), 2);
        assert_eq!(group.pane_count(), 3);
    }

    #[gpui::test]
    fn splitting_is_capped_at_max_panes(cx: &mut gpui::TestAppContext) {
        let (_, mut group) = group_with(cx, MAX_SPLIT_PANES);
        let extra = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        let anchor = group.active_or_first().unwrap();
        assert!(!group.split_pane(anchor.entity_id(), extra, Axis::Horizontal));
        assert_eq!(group.pane_count(), MAX_SPLIT_PANES);
    }

    #[gpui::test]
    fn removing_a_pane_collapses_its_axis(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 2);
        let third = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(terminals[1].entity_id(), third.clone(), Axis::Vertical);

        assert!(group.remove_pane(third.entity_id()));
        assert_eq!(group.pane_count(), 2);
        let SplitMember::Axis(root) = &group.root else {
            panic!("root should still be an axis");
        };
        assert_eq!(root.members.len(), 2);
        assert!(
            root.members
                .iter()
                .all(|member| matches!(member, SplitMember::Pane(_)))
        );
    }

    #[gpui::test]
    fn removing_the_last_pane_empties_the_group(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 1);
        assert!(!group.remove_pane(terminals[0].entity_id()));
        assert_eq!(group.pane_count(), 0);
    }

    #[gpui::test]
    fn pane_sizes_subtract_dividers_and_follow_flex_weights(cx: &mut gpui::TestAppContext) {
        let (terminals, mut group) = group_with(cx, 1);
        let second = cx.update(|cx| TerminalView::inert_test_view_entity(cx));
        group.split_pane(terminals[0].entity_id(), second, Axis::Horizontal);
        if let SplitMember::Axis(axis) = &mut group.root {
            axis.flexes = vec![3.0, 1.0];
        }

        let sizes = group.pane_sizes(405.0, 200.0);
        assert_eq!(sizes.len(), 2);
        // One divider, so 400 px split 3:1.
        for (id, size) in &sizes {
            assert_eq!(size.height, 200.0);
            let expected = if *id == terminals[0].entity_id() {
                300.0
            } else {
                100.0
            };
            assert!((size.width - expected).abs() < 0.01, "{size:?}");
        }
    }

    #[test]
    fn divider_drag_transfers_flex_within_min_pane_bounds() {
        let drag = SplitDividerDrag::new(
            0,
            Vec::new(),
            0,
            Axis::Horizontal,
            100.0,
            50.0,
            300.0,
            100.0,
            100.0,
        );
        // Dragging past the second pane's minimum leaves it at the minimum.
        let (first, second) = drag.flexes_for(500.0, 50.0);
        assert!((first * 100.0 - 320.0).abs() < 0.01);
        assert!((second * 100.0 - 80.0).abs() < 0.01);
        // Dragging back to the pair's midpoint balances it.
        let (first, second) = drag.flexes_for(0.0, 50.0);
        assert!((first * 100.0 - 200.0).abs() < 0.01);
        assert!((second * 100.0 - 200.0).abs() < 0.01);
    }
}
