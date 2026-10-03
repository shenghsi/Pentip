use std::sync::Arc;

use gpui::{Axis, Bounds, Pixels};
use parking_lot::Mutex;
use workspace::SplitDirection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentPaneId(usize);

pub(crate) enum AgentPaneMember {
    Pane(AgentPaneId),
    Axis(AgentPaneAxis),
}

pub(crate) struct AgentPaneAxis {
    pub axis: Axis,
    pub members: Vec<AgentPaneMember>,
    pub flexes: Arc<Mutex<Vec<f32>>>,
    pub bounding_boxes: Arc<Mutex<Vec<Option<Bounds<Pixels>>>>>,
}

impl AgentPaneAxis {
    fn new(axis: Axis, members: Vec<AgentPaneMember>) -> Self {
        let flexes = Arc::new(Mutex::new(vec![1.; members.len()]));
        let bounding_boxes = Arc::new(Mutex::new(vec![None; members.len()]));
        Self {
            axis,
            members,
            flexes,
            bounding_boxes,
        }
    }

    fn reset_sizes(&mut self) {
        *self.flexes.lock() = vec![1.; self.members.len()];
        *self.bounding_boxes.lock() = vec![None; self.members.len()];
    }
}

/// The split tree of the Agent Panel. Each leaf is one pane that shows one
/// entry (an agent thread or an agent terminal).
pub(crate) struct AgentPaneLayout {
    root: AgentPaneMember,
    next_pane_id: usize,
}

impl AgentPaneLayout {
    pub fn new() -> (Self, AgentPaneId) {
        let first_pane = AgentPaneId(0);
        (
            Self {
                root: AgentPaneMember::Pane(first_pane),
                next_pane_id: 1,
            },
            first_pane,
        )
    }

    pub fn root(&self) -> &AgentPaneMember {
        &self.root
    }

    pub fn panes(&self) -> Vec<AgentPaneId> {
        let mut panes = Vec::new();
        collect_panes(&self.root, &mut panes);
        panes
    }

    pub fn pane_count(&self) -> usize {
        self.panes().len()
    }

    pub fn contains(&self, pane: AgentPaneId) -> bool {
        self.panes().contains(&pane)
    }

    /// Adds a new pane adjacent to `pane` and returns the id of the new pane.
    pub fn split(&mut self, pane: AgentPaneId, direction: SplitDirection) -> Option<AgentPaneId> {
        if !self.contains(pane) {
            return None;
        }
        let new_pane = AgentPaneId(self.next_pane_id);
        self.next_pane_id += 1;
        split_member(&mut self.root, pane, new_pane, direction);
        Some(new_pane)
    }

    /// Removes `pane` from the tree. The last pane cannot be removed.
    pub fn remove(&mut self, pane: AgentPaneId) -> bool {
        if matches!(self.root, AgentPaneMember::Pane(_)) || !self.contains(pane) {
            return false;
        }
        remove_member(&mut self.root, pane);
        true
    }

    pub fn swap(&mut self, first: AgentPaneId, second: AgentPaneId) {
        swap_members(&mut self.root, first, second);
    }

    pub fn pane_in_direction(
        &self,
        pane: AgentPaneId,
        direction: SplitDirection,
    ) -> Option<AgentPaneId> {
        let mut path = Vec::new();
        if !find_path(&self.root, pane, &mut path) {
            return None;
        }

        while let Some(child_index) = path.pop() {
            let AgentPaneMember::Axis(axis) = member_at(&self.root, &path)? else {
                continue;
            };
            if axis.axis != direction.axis() {
                continue;
            }
            let neighbor_index = if direction.increasing() {
                child_index.checked_add(1)
            } else {
                child_index.checked_sub(1)
            };
            if let Some(neighbor) = neighbor_index.and_then(|index| axis.members.get(index)) {
                return nearest_pane(neighbor, direction);
            }
        }
        None
    }
}

fn collect_panes(member: &AgentPaneMember, panes: &mut Vec<AgentPaneId>) {
    match member {
        AgentPaneMember::Pane(pane) => panes.push(*pane),
        AgentPaneMember::Axis(axis) => {
            for member in &axis.members {
                collect_panes(member, panes);
            }
        }
    }
}

fn split_member(
    member: &mut AgentPaneMember,
    pane: AgentPaneId,
    new_pane: AgentPaneId,
    direction: SplitDirection,
) -> bool {
    match member {
        AgentPaneMember::Pane(existing) => {
            if *existing != pane {
                return false;
            }
            let members = if direction.increasing() {
                vec![AgentPaneMember::Pane(pane), AgentPaneMember::Pane(new_pane)]
            } else {
                vec![AgentPaneMember::Pane(new_pane), AgentPaneMember::Pane(pane)]
            };
            *member = AgentPaneMember::Axis(AgentPaneAxis::new(direction.axis(), members));
            true
        }
        AgentPaneMember::Axis(axis) => {
            let direct_child = axis.members.iter().position(
                |member| matches!(member, AgentPaneMember::Pane(existing) if *existing == pane),
            );
            if let Some(index) = direct_child
                && axis.axis == direction.axis()
            {
                let insert_index = if direction.increasing() {
                    index + 1
                } else {
                    index
                };
                axis.members
                    .insert(insert_index, AgentPaneMember::Pane(new_pane));
                axis.reset_sizes();
                return true;
            }
            axis.members
                .iter_mut()
                .any(|member| split_member(member, pane, new_pane, direction))
        }
    }
}

fn remove_member(member: &mut AgentPaneMember, pane: AgentPaneId) {
    let AgentPaneMember::Axis(axis) = member else {
        return;
    };
    let member_count = axis.members.len();
    axis.members
        .retain(|member| !matches!(member, AgentPaneMember::Pane(existing) if *existing == pane));
    if axis.members.len() != member_count {
        axis.reset_sizes();
    } else {
        for member in &mut axis.members {
            remove_member(member, pane);
        }
    }
    if axis.members.len() == 1
        && let Some(only_member) = axis.members.pop()
    {
        *member = only_member;
    }
}

fn swap_members(member: &mut AgentPaneMember, first: AgentPaneId, second: AgentPaneId) {
    match member {
        AgentPaneMember::Pane(pane) => {
            if *pane == first {
                *pane = second;
            } else if *pane == second {
                *pane = first;
            }
        }
        AgentPaneMember::Axis(axis) => {
            for member in &mut axis.members {
                swap_members(member, first, second);
            }
        }
    }
}

fn find_path(member: &AgentPaneMember, pane: AgentPaneId, path: &mut Vec<usize>) -> bool {
    match member {
        AgentPaneMember::Pane(existing) => *existing == pane,
        AgentPaneMember::Axis(axis) => {
            for (index, member) in axis.members.iter().enumerate() {
                path.push(index);
                if find_path(member, pane, path) {
                    return true;
                }
                path.pop();
            }
            false
        }
    }
}

fn member_at<'a>(root: &'a AgentPaneMember, path: &[usize]) -> Option<&'a AgentPaneMember> {
    let mut member = root;
    for index in path {
        let AgentPaneMember::Axis(axis) = member else {
            return None;
        };
        member = axis.members.get(*index)?;
    }
    Some(member)
}

/// The pane of `member` that is nearest to a pane that moves in `direction`
/// into `member`.
fn nearest_pane(member: &AgentPaneMember, direction: SplitDirection) -> Option<AgentPaneId> {
    let mut member = member;
    loop {
        match member {
            AgentPaneMember::Pane(pane) => return Some(*pane),
            AgentPaneMember::Axis(axis) => {
                let enter_from_end = axis.axis == direction.axis() && !direction.increasing();
                member = if enter_from_end {
                    axis.members.last()?
                } else {
                    axis.members.first()?
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_adds_panes_in_the_requested_direction() {
        let (mut layout, first) = AgentPaneLayout::new();
        let right = layout.split(first, SplitDirection::Right).unwrap();
        let left = layout.split(first, SplitDirection::Left).unwrap();
        let below = layout.split(right, SplitDirection::Down).unwrap();

        assert_eq!(layout.panes(), vec![left, first, right, below]);
        assert_eq!(
            layout.pane_in_direction(first, SplitDirection::Left),
            Some(left)
        );
        assert_eq!(
            layout.pane_in_direction(first, SplitDirection::Right),
            Some(right)
        );
        assert_eq!(
            layout.pane_in_direction(right, SplitDirection::Down),
            Some(below)
        );
        assert_eq!(
            layout.pane_in_direction(below, SplitDirection::Left),
            Some(first)
        );
        assert_eq!(layout.pane_in_direction(left, SplitDirection::Left), None);
        assert_eq!(layout.pane_in_direction(first, SplitDirection::Up), None);
    }

    #[test]
    fn remove_collapses_an_axis_with_one_member() {
        let (mut layout, first) = AgentPaneLayout::new();
        assert!(!layout.remove(first));

        let right = layout.split(first, SplitDirection::Right).unwrap();
        let below = layout.split(right, SplitDirection::Down).unwrap();

        assert!(layout.remove(right));
        assert_eq!(layout.panes(), vec![first, below]);
        assert_eq!(
            layout.pane_in_direction(first, SplitDirection::Right),
            Some(below)
        );

        assert!(layout.remove(first));
        assert!(matches!(layout.root(), AgentPaneMember::Pane(pane) if *pane == below));
        assert!(!layout.remove(below));
    }

    #[test]
    fn swap_exchanges_two_panes() {
        let (mut layout, first) = AgentPaneLayout::new();
        let right = layout.split(first, SplitDirection::Right).unwrap();

        layout.swap(first, right);

        assert_eq!(layout.panes(), vec![right, first]);
    }
}
