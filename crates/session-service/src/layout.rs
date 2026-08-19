//! Pure pane-layout tree operations over the desired-state snapshot.
use std::collections::HashSet;

use hh_protocol::{DropPlacement, Pane, PaneLayout, SessionSnapshot, SplitAxis, Workspace};
use uuid::Uuid;

pub(crate) fn pane_ids_in_snapshot(snapshot: &SessionSnapshot) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    for workspace in &snapshot.workspaces {
        for tab in &workspace.tabs {
            collect_pane_ids(&tab.layout, &mut pane_ids);
        }
    }
    pane_ids
}

pub(crate) fn pane_ids_for_workspace(workspace: &Workspace) -> Vec<Uuid> {
    let mut pane_ids = Vec::new();
    for tab in &workspace.tabs {
        collect_pane_ids(&tab.layout, &mut pane_ids);
    }
    pane_ids
}

pub(crate) fn collect_pane_ids(layout: &PaneLayout, pane_ids: &mut Vec<Uuid>) {
    match layout {
        PaneLayout::Leaf { pane } => pane_ids.push(pane.id),
        PaneLayout::Stack { panes, .. } => {
            pane_ids.extend(panes.iter().map(|pane| pane.id));
        }
        PaneLayout::Split { first, second, .. } => {
            collect_pane_ids(first, pane_ids);
            collect_pane_ids(second, pane_ids);
        }
    }
}

pub(crate) fn first_pane_id(snapshot: &SessionSnapshot) -> Option<Uuid> {
    fn first(layout: &PaneLayout) -> Uuid {
        match layout {
            PaneLayout::Leaf { pane } => pane.id,
            PaneLayout::Stack { active, .. } => *active,
            PaneLayout::Split { first: layout, .. } => first(layout),
        }
    }
    snapshot
        .workspaces
        .first()
        .and_then(|workspace| workspace.tabs.first())
        .map(|tab| first(&tab.layout))
}

pub(crate) fn find_pane_mut_in_snapshot(
    snapshot: &mut SessionSnapshot,
    pane_id: Uuid,
) -> Option<&mut Pane> {
    snapshot
        .workspaces
        .iter_mut()
        .flat_map(|workspace| workspace.tabs.iter_mut())
        .find_map(|tab| find_pane_mut(&mut tab.layout, pane_id))
}

pub(crate) fn find_pane_in_snapshot(snapshot: &SessionSnapshot, pane_id: Uuid) -> Option<&Pane> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .find_map(|tab| find_pane(&tab.layout, pane_id))
}

pub(crate) fn find_pane(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane(first, pane_id).or_else(|| find_pane(second, pane_id))
        }
    }
}

pub(crate) fn find_pane_mut(layout: &mut PaneLayout, pane_id: Uuid) -> Option<&mut Pane> {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => Some(pane),
        PaneLayout::Leaf { .. } => None,
        PaneLayout::Stack { panes, .. } => panes.iter_mut().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            find_pane_mut(first, pane_id).or_else(|| find_pane_mut(second, pane_id))
        }
    }
}

pub(crate) fn split_layout(
    layout: &mut PaneLayout,
    target: Uuid,
    pane: Pane,
    axis: SplitAxis,
) -> bool {
    match layout {
        PaneLayout::Leaf { pane: existing } if existing.id == target => {
            let first = layout.clone();
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(PaneLayout::Leaf { pane }),
            };
            true
        }
        PaneLayout::Stack { panes, .. } if panes.iter().any(|existing| existing.id == target) => {
            let first = layout.clone();
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(PaneLayout::Leaf { pane }),
            };
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            split_layout(first, target, pane.clone(), axis)
                || split_layout(second, target, pane, axis)
        }
    }
}

pub(crate) fn add_tab(layout: &mut PaneLayout, target: Uuid, pane: Pane) -> bool {
    match layout {
        PaneLayout::Leaf { pane: existing } if existing.id == target => {
            let existing = existing.clone();
            let active = pane.id;
            *layout = PaneLayout::Stack {
                panes: vec![existing, pane],
                active,
            };
            true
        }
        PaneLayout::Stack { panes, active } if panes.iter().any(|pane| pane.id == target) => {
            *active = pane.id;
            panes.push(pane);
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            add_tab(first, target, pane.clone()) || add_tab(second, target, pane)
        }
    }
}

pub(crate) fn activate_tab(layout: &mut PaneLayout, pane_id: Uuid) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => pane.id == pane_id,
        PaneLayout::Stack { panes, active } if panes.iter().any(|pane| pane.id == pane_id) => {
            *active = pane_id;
            true
        }
        PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            activate_tab(first, pane_id) || activate_tab(second, pane_id)
        }
    }
}

pub(crate) fn layout_contains(layout: &PaneLayout, pane_id: Uuid) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => pane.id == pane_id,
        PaneLayout::Stack { panes, .. } => panes.iter().any(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            layout_contains(first, pane_id) || layout_contains(second, pane_id)
        }
    }
}

pub(crate) fn first_layout_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { active, .. } => *active,
        PaneLayout::Split { first, .. } => first_layout_pane(first),
    }
}

/// Removes runtime-only (tmux) panes from a layout that is about to be
/// persisted, collapsing the nodes they vacate. Returns false when nothing of
/// the layout survives, so the caller drops the tab entirely.
pub(crate) fn retain_persistable_panes(layout: &mut PaneLayout, dropped: &HashSet<Uuid>) -> bool {
    match layout {
        PaneLayout::Leaf { pane } => !dropped.contains(&pane.id),
        PaneLayout::Stack { panes, active } => {
            panes.retain(|pane| !dropped.contains(&pane.id));
            if panes.len() >= 2 {
                if !panes.iter().any(|pane| pane.id == *active) {
                    *active = panes[0].id;
                }
                return true;
            }
            let sole = panes.first().cloned();
            match sole {
                Some(pane) => {
                    *layout = PaneLayout::Leaf { pane };
                    true
                }
                None => false,
            }
        }
        PaneLayout::Split { first, second, .. } => {
            let keep_first = retain_persistable_panes(first, dropped);
            let keep_second = retain_persistable_panes(second, dropped);
            match (keep_first, keep_second) {
                (true, true) => true,
                (true, false) => {
                    let survivor = (**first).clone();
                    *layout = survivor;
                    true
                }
                (false, true) => {
                    let survivor = (**second).clone();
                    *layout = survivor;
                    true
                }
                (false, false) => false,
            }
        }
    }
}

pub(crate) fn workspace_id_for_pane(snapshot: &SessionSnapshot, pane_id: Uuid) -> Option<Uuid> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| layout_contains(&tab.layout, pane_id))
        })
        .map(|workspace| workspace.id)
}

pub(crate) fn move_workspace_pane_to_split(
    workspace: &mut Workspace,
    source: Uuid,
    target: Uuid,
    placement: DropPlacement,
) -> bool {
    let Some(source_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, source))
    else {
        return false;
    };
    let Some(target_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, target))
    else {
        return false;
    };
    if source_tab == target_tab {
        return move_existing_pane_to_split(
            &mut workspace.tabs[source_tab].layout,
            source,
            target,
            placement,
        );
    }

    let (Some(pane), remaining) = detach_pane(workspace.tabs[source_tab].layout.clone(), source)
    else {
        return false;
    };
    let mut target_layout = workspace.tabs[target_tab].layout.clone();
    if !insert_split(&mut target_layout, target, pane, placement) {
        return false;
    }
    workspace.tabs[target_tab].layout = target_layout;
    if let Some(remaining) = remaining {
        workspace.tabs[source_tab].layout = remaining;
    } else {
        workspace.tabs.remove(source_tab);
    }
    true
}

pub(crate) fn move_workspace_pane_to_tab(
    workspace: &mut Workspace,
    source: Uuid,
    target: Uuid,
) -> bool {
    let Some(source_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, source))
    else {
        return false;
    };
    let Some(target_tab) = workspace
        .tabs
        .iter()
        .position(|tab| layout_contains(&tab.layout, target))
    else {
        return false;
    };
    if source_tab == target_tab {
        return move_existing_pane_to_tab(&mut workspace.tabs[source_tab].layout, source, target);
    }

    let (Some(pane), remaining) = detach_pane(workspace.tabs[source_tab].layout.clone(), source)
    else {
        return false;
    };
    let mut target_layout = workspace.tabs[target_tab].layout.clone();
    if !add_tab(&mut target_layout, target, pane) {
        return false;
    }
    workspace.tabs[target_tab].layout = target_layout;
    if let Some(remaining) = remaining {
        workspace.tabs[source_tab].layout = remaining;
    } else {
        workspace.tabs.remove(source_tab);
    }
    true
}

pub(crate) fn move_existing_pane_to_split(
    layout: &mut PaneLayout,
    source: Uuid,
    target: Uuid,
    placement: DropPlacement,
) -> bool {
    if !layout_contains(layout, source) || !layout_contains(layout, target) || source == target {
        return false;
    }
    let original = layout.clone();
    let (pane, remaining) = detach_pane(original, source);
    let (Some(pane), Some(mut remaining)) = (pane, remaining) else {
        return false;
    };
    if !insert_split(&mut remaining, target, pane, placement) {
        return false;
    }
    *layout = remaining;
    true
}

pub(crate) fn move_existing_pane_to_tab(
    layout: &mut PaneLayout,
    source: Uuid,
    target: Uuid,
) -> bool {
    if !layout_contains(layout, source) || !layout_contains(layout, target) || source == target {
        return false;
    }
    let original = layout.clone();
    let (pane, remaining) = detach_pane(original, source);
    let (Some(pane), Some(mut remaining)) = (pane, remaining) else {
        return false;
    };
    if !add_tab(&mut remaining, target, pane) {
        return false;
    }
    *layout = remaining;
    true
}

pub(crate) fn split_lone_layout_with_replacement(
    layout: &mut PaneLayout,
    pane_id: Uuid,
    replacement: Pane,
    placement: DropPlacement,
) -> bool {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == pane_id => {
            let moved = layout.clone();
            let replacement = PaneLayout::Leaf { pane: replacement };
            let (axis, moved_first) = match placement {
                DropPlacement::Left => (SplitAxis::Horizontal, true),
                DropPlacement::Right => (SplitAxis::Horizontal, false),
                DropPlacement::Top => (SplitAxis::Vertical, true),
                DropPlacement::Bottom => (SplitAxis::Vertical, false),
            };
            let (first, second) = if moved_first {
                (moved, replacement)
            } else {
                (replacement, moved)
            };
            *layout = PaneLayout::Split {
                axis,
                ratio: 0.5,
                first: Box::new(first),
                second: Box::new(second),
            };
            true
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
        PaneLayout::Split { first, second, .. } => {
            split_lone_layout_with_replacement(first, pane_id, replacement.clone(), placement)
                || split_lone_layout_with_replacement(second, pane_id, replacement, placement)
        }
    }
}

pub(crate) fn detach_pane(layout: PaneLayout, source: Uuid) -> (Option<Pane>, Option<PaneLayout>) {
    match layout {
        PaneLayout::Leaf { pane } if pane.id == source => (Some(pane), None),
        PaneLayout::Leaf { pane } => (None, Some(PaneLayout::Leaf { pane })),
        PaneLayout::Stack { mut panes, active } => {
            let Some(index) = panes.iter().position(|pane| pane.id == source) else {
                return (None, Some(PaneLayout::Stack { panes, active }));
            };
            let pane = panes.remove(index);
            let remaining = match panes.len() {
                0 => None,
                1 => Some(PaneLayout::Leaf {
                    pane: panes.remove(0),
                }),
                _ => {
                    let active = if active == source {
                        panes[0].id
                    } else {
                        active
                    };
                    Some(PaneLayout::Stack { panes, active })
                }
            };
            (Some(pane), remaining)
        }
        PaneLayout::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (pane, first_remaining) = detach_pane(*first, source);
            if pane.is_some() {
                let layout = match first_remaining {
                    Some(first) => PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second,
                    },
                    None => *second,
                };
                return (pane, Some(layout));
            }
            let (pane, second_remaining) = detach_pane(*second, source);
            if pane.is_some() {
                let first = first_remaining.expect("unchanged first");
                let layout = match second_remaining {
                    Some(second) => PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first),
                        second: Box::new(second),
                    },
                    None => first,
                };
                (pane, Some(layout))
            } else {
                (
                    None,
                    Some(PaneLayout::Split {
                        axis,
                        ratio,
                        first: Box::new(first_remaining.expect("unchanged first")),
                        second: Box::new(second_remaining.expect("unchanged second")),
                    }),
                )
            }
        }
    }
}

pub(crate) fn insert_split(
    layout: &mut PaneLayout,
    target: Uuid,
    pane: Pane,
    placement: DropPlacement,
) -> bool {
    let is_target = match layout {
        PaneLayout::Leaf { pane } => pane.id == target,
        PaneLayout::Stack { panes, .. } => panes.iter().any(|pane| pane.id == target),
        PaneLayout::Split { .. } => false,
    };
    if is_target {
        let existing = layout.clone();
        let incoming = PaneLayout::Leaf { pane };
        let (axis, incoming_first) = match placement {
            DropPlacement::Left => (SplitAxis::Horizontal, true),
            DropPlacement::Right => (SplitAxis::Horizontal, false),
            DropPlacement::Top => (SplitAxis::Vertical, true),
            DropPlacement::Bottom => (SplitAxis::Vertical, false),
        };
        let (first, second) = if incoming_first {
            (incoming, existing)
        } else {
            (existing, incoming)
        };
        *layout = PaneLayout::Split {
            axis,
            ratio: 0.5,
            first: Box::new(first),
            second: Box::new(second),
        };
        return true;
    }
    match layout {
        PaneLayout::Split { first, second, .. } => {
            insert_split(first, target, pane.clone(), placement)
                || insert_split(second, target, pane, placement)
        }
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => false,
    }
}

pub(crate) fn swap_pane_ids(layout: &mut PaneLayout, source: Uuid, target: Uuid) {
    let swap_id = |id: &mut Uuid| {
        if *id == source {
            *id = target;
        } else if *id == target {
            *id = source;
        }
    };
    match layout {
        PaneLayout::Leaf { pane } => swap_id(&mut pane.id),
        PaneLayout::Stack { panes, active } => {
            for pane in panes {
                swap_id(&mut pane.id);
            }
            swap_id(active);
        }
        PaneLayout::Split { first, second, .. } => {
            swap_pane_ids(first, source, target);
            swap_pane_ids(second, source, target);
        }
    }
}

#[cfg(test)]
use hh_protocol::{PaneKind, TerminalIdentity};

#[cfg(test)]
pub(crate) fn pane_fixture(id: Uuid) -> Pane {
    Pane {
        kind: PaneKind::Terminal,
        id,
        title: format!("Terminal {id}"),
        shell: "shell".to_owned(),
        color: None,
        identity: TerminalIdentity::default(),
        custom_title: None,
        profile_override: None,
        custom_icon: None,
    }
}

#[cfg(test)]
pub(crate) fn tab_id_for_pane(snapshot: &SessionSnapshot, pane_id: Uuid) -> Uuid {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| workspace.tabs.iter())
        .find(|tab| layout_contains(&tab.layout, pane_id))
        .map(|tab| tab.id)
        .unwrap()
}

#[cfg(test)]
pub(crate) fn first_pane_in_layout(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { active, .. } => *active,
        PaneLayout::Split { first, .. } => first_pane_in_layout(first),
    }
}

#[cfg(test)]
pub(crate) fn pane_in_layout(layout: &PaneLayout, pane_id: Uuid) -> Option<&Pane> {
    match layout {
        PaneLayout::Leaf { pane } => (pane.id == pane_id).then_some(pane),
        PaneLayout::Stack { panes, .. } => panes.iter().find(|pane| pane.id == pane_id),
        PaneLayout::Split { first, second, .. } => {
            pane_in_layout(first, pane_id).or_else(|| pane_in_layout(second, pane_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::layout::{pane_fixture, retain_persistable_panes};

    #[cfg(test)]
    #[test]
    fn persistable_pane_pruning_collapses_invalid_layout_shapes() {
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let third_id = Uuid::new_v4();

        let mut leaf = PaneLayout::Leaf {
            pane: pane_fixture(first_id),
        };
        assert!(!retain_persistable_panes(
            &mut leaf,
            &HashSet::from([first_id])
        ));

        let second = pane_fixture(second_id);
        let mut two_pane_stack = PaneLayout::Stack {
            panes: vec![pane_fixture(first_id), second.clone()],
            active: first_id,
        };
        assert!(retain_persistable_panes(
            &mut two_pane_stack,
            &HashSet::from([first_id])
        ));
        assert_eq!(two_pane_stack, PaneLayout::Leaf { pane: second });

        let first = pane_fixture(first_id);
        let third = pane_fixture(third_id);
        let mut three_pane_stack = PaneLayout::Stack {
            panes: vec![first.clone(), pane_fixture(second_id), third.clone()],
            active: second_id,
        };
        assert!(retain_persistable_panes(
            &mut three_pane_stack,
            &HashSet::from([second_id])
        ));
        assert_eq!(
            three_pane_stack,
            PaneLayout::Stack {
                panes: vec![first.clone(), third.clone()],
                active: first_id,
            }
        );

        let mut one_sided_split = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: pane_fixture(second_id),
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: third.clone(),
            }),
        };
        assert!(retain_persistable_panes(
            &mut one_sided_split,
            &HashSet::from([second_id])
        ));
        assert_eq!(one_sided_split, PaneLayout::Leaf { pane: third });

        let mut empty_split = PaneLayout::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf { pane: first }),
            second: Box::new(PaneLayout::Leaf {
                pane: pane_fixture(second_id),
            }),
        };
        assert!(!retain_persistable_panes(
            &mut empty_split,
            &HashSet::from([first_id, second_id])
        ));
    }
}
