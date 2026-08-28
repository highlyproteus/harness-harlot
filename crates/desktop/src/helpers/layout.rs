use crate::view_models::{LayoutControlMutation, SplitControlId};
use hh_protocol::{Pane, PaneLayout, Workspace};
use std::collections::HashMap;
use uuid::Uuid;

pub(crate) fn visible_panes(layout: &PaneLayout) -> Vec<Uuid> {
    match layout {
        PaneLayout::Leaf { pane } => vec![pane.id],
        PaneLayout::Stack { active, .. } => vec![*active],
        PaneLayout::Split { first, second, .. } => {
            let mut panes = visible_panes(first);
            panes.extend(visible_panes(second));
            panes
        }
    }
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

pub(crate) fn inactive_stack_contains(layout: &PaneLayout, pane_id: Uuid) -> bool {
    match layout {
        PaneLayout::Leaf { .. } => false,
        PaneLayout::Stack { panes, active } => {
            *active != pane_id && panes.iter().any(|pane| pane.id == pane_id)
        }
        PaneLayout::Split { first, second, .. } => {
            inactive_stack_contains(first, pane_id) || inactive_stack_contains(second, pane_id)
        }
    }
}

pub(crate) fn collect_terminal_tabs<'a>(layout: &'a PaneLayout, panes: &mut Vec<&'a Pane>) {
    match layout {
        PaneLayout::Leaf { pane } => panes.push(pane),
        PaneLayout::Stack { panes: stacked, .. } => panes.extend(stacked),
        PaneLayout::Split { first, second, .. } => {
            collect_terminal_tabs(first, panes);
            collect_terminal_tabs(second, panes);
        }
    }
}

pub(crate) fn workspace_terminal_tabs(workspace: &Workspace) -> Vec<&Pane> {
    let mut panes = Vec::new();
    for tab in &workspace.tabs {
        collect_terminal_tabs(&tab.layout, &mut panes);
    }
    panes
}

/// Visible panes across every tab of one workstation, in tab order.
///
/// Focus bookkeeping must reason about the whole workstation: a runtime-only
/// tmux tab is never the first tab, so scoping this to `tabs.first()` would
/// treat a perfectly live focused pane as gone and snap the viewport back to
/// the initial terminal on the next poll.
pub(crate) fn workspace_visible_panes(workspace: &Workspace) -> Vec<Uuid> {
    workspace
        .tabs
        .iter()
        .flat_map(|tab| visible_panes(&tab.layout))
        .collect()
}

pub(crate) fn workspace_layout_for_focused_pane(
    workspace: &Workspace,
    focused_pane: Option<Uuid>,
) -> Option<&PaneLayout> {
    focused_pane
        .and_then(|pane_id| {
            workspace
                .tabs
                .iter()
                .find(|tab| find_pane(&tab.layout, pane_id).is_some())
                .map(|tab| &tab.layout)
        })
        .or_else(|| workspace.tabs.first().map(|tab| &tab.layout))
}

pub(crate) fn stable_representative_pane(layout: &PaneLayout) -> Uuid {
    match layout {
        PaneLayout::Leaf { pane } => pane.id,
        PaneLayout::Stack { panes, active } => panes.first().map_or(*active, |pane| pane.id),
        PaneLayout::Split { first, .. } => stable_representative_pane(first),
    }
}

pub(crate) fn split_control_id(first: &PaneLayout, second: &PaneLayout) -> SplitControlId {
    SplitControlId {
        first: stable_representative_pane(first),
        second: stable_representative_pane(second),
    }
}

pub(crate) fn zoom_projection(layout: &PaneLayout, pane_id: Uuid) -> Option<PaneLayout> {
    match layout {
        PaneLayout::Leaf { pane } => (pane.id == pane_id).then(|| layout.clone()),
        PaneLayout::Stack { panes, .. } => panes
            .iter()
            .any(|pane| pane.id == pane_id)
            .then(|| layout.clone()),
        PaneLayout::Split { first, second, .. } => {
            zoom_projection(first, pane_id).or_else(|| zoom_projection(second, pane_id))
        }
    }
}

pub(crate) fn apply_layout_control_mutation(
    layout: &PaneLayout,
    ratios: &mut HashMap<SplitControlId, f32>,
    mutation: LayoutControlMutation,
) -> usize {
    match layout {
        PaneLayout::Leaf { .. } | PaneLayout::Stack { .. } => 0,
        PaneLayout::Split { first, second, .. } => {
            match mutation {
                LayoutControlMutation::Equalize => {
                    ratios.insert(split_control_id(first, second), 0.5);
                }
            }
            1 + apply_layout_control_mutation(first, ratios, mutation)
                + apply_layout_control_mutation(second, ratios, mutation)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HashMap, LayoutControlMutation, Pane, PaneLayout, SplitControlId, Uuid,
        apply_layout_control_mutation, visible_panes, workspace_layout_for_focused_pane,
        workspace_terminal_tabs, workspace_visible_panes, zoom_projection,
    };
    use crate::helpers::FocusResync;
    use crate::helpers::focus_resync_for;
    use crate::helpers::tab_identity_presentation;
    use crate::helpers::terminal_tab_count_label;

    use hh_protocol::SessionSnapshot;
    use hh_protocol::SplitAxis;
    use hh_protocol::TerminalProfile;

    #[test]
    fn workspace_rail_lists_every_terminal_tab_across_stacks_and_splits() {
        let make_pane = |id: u128, title: &str, profile: TerminalProfile| Pane {
            id: Uuid::from_u128(id),
            kind: hh_protocol::PaneKind::Terminal,
            title: title.to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity {
                profile,
                source: hh_protocol::TerminalIdentitySource::Command,
            },
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let codex = make_pane(1, "Codex review", TerminalProfile::Codex);
        let droid = make_pane(2, "Droid build", TerminalProfile::Droid);
        let terminal = make_pane(3, "Logs", TerminalProfile::Terminal);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs[0].layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Stack {
                panes: vec![codex.clone(), droid.clone()],
                active: droid.id,
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: terminal.clone(),
            }),
        };

        let tabs = workspace_terminal_tabs(&workspace);

        assert_eq!(
            tabs.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            vec![codex.id, droid.id, terminal.id]
        );
        assert_eq!(
            tabs.iter()
                .map(|pane| tab_identity_presentation(pane).profile)
                .collect::<Vec<_>>(),
            vec![
                TerminalProfile::Codex,
                TerminalProfile::Droid,
                TerminalProfile::Terminal
            ]
        );
        assert_eq!(terminal_tab_count_label(tabs.len()), "3 terminals");
    }

    #[test]
    fn runtime_tmux_tab_panes_stay_visible_to_focus_bookkeeping() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        let initial = visible_panes(&workspace.tabs[0].layout)[0];
        let tmux_pane = Uuid::from_u128(0x77);
        workspace.tabs.push(hh_protocol::Tab {
            id: Uuid::from_u128(0x88),
            title: "buzz".to_owned(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: tmux_pane,
                    kind: hh_protocol::PaneKind::Terminal,
                    title: "tmux buzz".to_owned(),
                    shell: "tmux".to_owned(),
                    color: None,
                    identity: hh_protocol::TerminalIdentity::default(),
                    status: hh_protocol::PaneStatus::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });

        // The attached tmux tab is never first, so first-tab-only bookkeeping
        // treated its pane as gone and snapped focus back to the SSH shell on
        // the very next poll, leaving the tmux tab unrenderable.
        let visible = workspace_visible_panes(&workspace);
        assert_eq!(visible, vec![initial, tmux_pane]);
        assert_eq!(
            focus_resync_for(&visible, Some(tmux_pane), true),
            FocusResync::Keep
        );
        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(tmux_pane)),
            Some(&workspace.tabs[1].layout)
        );

        // A pane that really vanished still falls back to the first tab, and an
        // empty workstation clears focus outright.
        assert_eq!(
            focus_resync_for(&visible, Some(Uuid::from_u128(0x99)), false),
            FocusResync::Switch(initial)
        );
        assert_eq!(
            focus_resync_for(&visible, None, false),
            FocusResync::Switch(initial)
        );
        assert_eq!(
            focus_resync_for(&[], Some(tmux_pane), false),
            FocusResync::Clear
        );
    }

    #[test]
    fn zoom_is_a_projection_that_does_not_mutate_canonical_layout() {
        let first = Pane {
            id: Uuid::from_u128(101),
            kind: hh_protocol::PaneKind::Terminal,
            title: "one".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let second = Pane {
            id: Uuid::from_u128(102),
            kind: hh_protocol::PaneKind::Terminal,
            title: "two".to_owned(),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            first: Box::new(PaneLayout::Leaf {
                pane: first.clone(),
            }),
            second: Box::new(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id,
            }),
        };
        let before = layout.clone();

        assert_eq!(
            zoom_projection(&layout, second.id),
            Some(PaneLayout::Stack {
                panes: vec![second.clone()],
                active: second.id
            })
        );
        assert_eq!(layout, before);
        assert_eq!(zoom_projection(&layout, Uuid::from_u128(999)), None);
    }

    #[test]
    fn equalize_is_a_controlled_mutation_over_all_current_split_identities() {
        let pane = |id| Pane {
            id: Uuid::from_u128(id),
            kind: hh_protocol::PaneKind::Terminal,
            title: format!("pane {id}"),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let nested = PaneLayout::Split {
            axis: SplitAxis::Vertical,
            ratio: 0.8,
            first: Box::new(PaneLayout::Leaf { pane: pane(2) }),
            second: Box::new(PaneLayout::Leaf { pane: pane(3) }),
        };
        let layout = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.2,
            first: Box::new(PaneLayout::Leaf { pane: pane(1) }),
            second: Box::new(nested),
        };
        let mut ratios = HashMap::from([
            (
                SplitControlId {
                    first: Uuid::from_u128(1),
                    second: Uuid::from_u128(2),
                },
                0.1,
            ),
            (
                SplitControlId {
                    first: Uuid::from_u128(2),
                    second: Uuid::from_u128(3),
                },
                0.9,
            ),
        ]);

        let changed =
            apply_layout_control_mutation(&layout, &mut ratios, LayoutControlMutation::Equalize);

        assert_eq!(changed, 2);
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(1),
                second: Uuid::from_u128(2)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
        assert!(
            (ratios[&SplitControlId {
                first: Uuid::from_u128(2),
                second: Uuid::from_u128(3)
            }] - 0.5)
                .abs()
                < f32::EPSILON
        );
    }
}
