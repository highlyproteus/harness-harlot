use crate::helpers::{collect_terminal_tabs, find_pane, visible_panes};

use hh_protocol::{AppearanceColor, Pane, PaneLayout, Workspace, WorkspaceConnection};
use std::collections::HashSet;
use uuid::Uuid;

/// Tabs shown in the persistent strip above the viewport: named or
/// multi-pane tabs first, then single-pane tabs, preserving insertion order
/// within each category.
pub(crate) fn workspace_tab_set(workspace: &Workspace) -> Vec<&hh_protocol::Tab> {
    let mut tabs = workspace.tabs.iter().collect::<Vec<_>>();
    tabs.sort_by_key(|tab| workspace_tab_rank(tab));
    tabs
}

pub(crate) fn workspace_tab_focus_target(
    tab: &hh_protocol::Tab,
    focused_pane: Option<Uuid>,
) -> Option<Uuid> {
    focused_pane
        .filter(|pane_id| find_pane(&tab.layout, *pane_id).is_some())
        .or_else(|| visible_panes(&tab.layout).first().copied())
}

/// Pane a strip-tab click should focus, resolved from the current snapshot.
pub(crate) fn workspace_tab_click_target(
    workspace: &Workspace,
    tab_id: Uuid,
    focused_pane: Option<Uuid>,
) -> Option<Uuid> {
    let tab = workspace.tabs.iter().find(|tab| tab.id == tab_id)?;
    workspace_tab_focus_target(tab, focused_pane)
}

/// Strip tab that should render as active: the tab holding the focused pane.
pub(crate) fn workspace_strip_active_tab(
    workspace: &Workspace,
    focused_pane: Option<Uuid>,
) -> Option<Uuid> {
    let pane_id = focused_pane?;
    workspace
        .tabs
        .iter()
        .find(|tab| find_pane(&tab.layout, pane_id).is_some())
        .map(|tab| tab.id)
}

pub(crate) fn workspace_tab_standalone_pane(tab: &hh_protocol::Tab) -> Option<&Pane> {
    if tab.custom_title.is_some() {
        return None;
    }
    let mut panes = Vec::new();
    collect_terminal_tabs(&tab.layout, &mut panes);
    (panes.len() == 1).then(|| panes[0])
}

fn pane_count(layout: &PaneLayout) -> usize {
    match layout {
        PaneLayout::Leaf { .. } => 1,
        PaneLayout::Stack { panes, .. } => panes.len(),
        PaneLayout::Split { first, second, .. } => pane_count(first) + pane_count(second),
    }
}

/// Tab display rank: named or multi-pane tabs first, then single panes.
fn workspace_tab_rank(tab: &hh_protocol::Tab) -> u8 {
    u8::from(tab.custom_title.is_none() && pane_count(&tab.layout) == 1)
}

/// One sidebar entry per tab. `label` is `Some` exactly when the tab renders
/// as a window of pane chips: it holds several panes, or the user named it.
pub(crate) struct WorkstationTabEntry<'a> {
    pub(crate) tab_id: Uuid,
    pub(crate) label: Option<&'a str>,
    pub(crate) color: Option<AppearanceColor>,
    pub(crate) pinned: bool,
    pub(crate) panes: Vec<&'a Pane>,
}

pub(crate) fn workspace_tab_entries(workspace: &Workspace) -> Vec<WorkstationTabEntry<'_>> {
    workspace_tab_set(workspace)
        .into_iter()
        .map(|tab| {
            let mut panes = Vec::new();
            collect_terminal_tabs(&tab.layout, &mut panes);
            let label = (panes.len() >= 2 || tab.custom_title.is_some())
                .then(|| tab.custom_title.as_deref().unwrap_or(tab.title.as_str()));
            WorkstationTabEntry {
                tab_id: tab.id,
                label,
                color: tab.color,
                pinned: tab.pinned,
                panes,
            }
        })
        .collect()
}

/// Sidebar display partitions inside one workstation: pinned tabs, then the
/// rest; relative order kept.
pub(crate) fn partition_workstation_entries(
    entries: Vec<WorkstationTabEntry<'_>>,
) -> (Vec<WorkstationTabEntry<'_>>, Vec<WorkstationTabEntry<'_>>) {
    entries.into_iter().partition(|entry| entry.pinned)
}

/// Whether `workspace` sits at the top of the sidebar tree: it has no parent,
/// or its parent is missing from the snapshot.
fn is_top_level_workstation(workspaces: &[Workspace], workspace: &Workspace) -> bool {
    workspace.parent_workstation.is_none_or(|parent| {
        !workspaces
            .iter()
            .any(|candidate| candidate.id == parent && !candidate.is_bot())
    })
}

/// Non-bot workstations directly under `parent` (the top level for `None`),
/// in sidebar order: pinned first, then by manual order.
pub(crate) fn child_workstations(
    workspaces: &[Workspace],
    parent: Option<Uuid>,
) -> Vec<&Workspace> {
    let mut children = workspaces
        .iter()
        .filter(|workspace| !workspace.is_bot())
        .filter(|workspace| match parent {
            Some(parent) => workspace.parent_workstation == Some(parent),
            None => is_top_level_workstation(workspaces, workspace),
        })
        .collect::<Vec<_>>();
    children.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
    children
}

/// Whether two workstations run on the same machine: both local, or both
/// reached through the same SSH destination.
pub(crate) fn same_machine(first: &WorkspaceConnection, second: &WorkspaceConnection) -> bool {
    match (first, second) {
        (WorkspaceConnection::Local, WorkspaceConnection::Local) => true,
        (
            WorkspaceConnection::SystemSsh {
                destination: first, ..
            },
            WorkspaceConnection::SystemSsh {
                destination: second,
                ..
            },
        ) => first == second,
        _ => false,
    }
}

/// Top-level workstation enclosing `id` (itself when top-level).
pub(crate) fn top_level_workstation(workspaces: &[Workspace], id: Uuid) -> Option<Uuid> {
    let mut current = workspaces.iter().find(|workspace| workspace.id == id)?;
    for _ in 0..workspaces.len() {
        if is_top_level_workstation(workspaces, current) {
            return Some(current.id);
        }
        let parent = current.parent_workstation?;
        current = workspaces.iter().find(|workspace| workspace.id == parent)?;
    }
    None
}

/// The visible sidebar tree, depth first: each workstation with its depth
/// (1 for top level), followed by its nested workstations only when it is
/// expanded.
pub(crate) fn visible_workstation_tree<'a>(
    workspaces: &'a [Workspace],
    expanded: &HashSet<Uuid>,
) -> Vec<(&'a Workspace, usize)> {
    fn visit<'a>(
        workspaces: &'a [Workspace],
        expanded: &HashSet<Uuid>,
        parent: Option<Uuid>,
        depth: usize,
        rows: &mut Vec<(&'a Workspace, usize)>,
    ) {
        for workspace in child_workstations(workspaces, parent) {
            if rows.iter().any(|(seen, _)| seen.id == workspace.id) {
                continue;
            }
            rows.push((workspace, depth));
            if expanded.contains(&workspace.id) {
                visit(workspaces, expanded, Some(workspace.id), depth + 1, rows);
            }
        }
    }
    let mut rows = Vec::new();
    visit(workspaces, expanded, None, 1, &mut rows);
    rows
}

/// Outcome of reconciling the focused pane against a fresh snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FocusResync {
    /// The focused pane is still on screen somewhere in the workstation.
    Keep,
    /// The pane still exists, but a stale snapshot shows a sibling stack tab.
    Reassert(Uuid),
    /// The focused pane is gone; fall back to the workstation's first pane.
    Switch(Uuid),
    /// The workstation has no visible pane left.
    Clear,
}

pub(crate) fn focus_resync_for(
    visible: &[Uuid],
    focused: Option<Uuid>,
    focused_exists: bool,
) -> FocusResync {
    if focused.is_some_and(|pane_id| visible.contains(&pane_id)) {
        return FocusResync::Keep;
    }
    if let Some(pane_id) = focused.filter(|_| focused_exists) {
        return FocusResync::Reassert(pane_id);
    }
    visible
        .first()
        .copied()
        .map_or(FocusResync::Clear, FocusResync::Switch)
}

pub(crate) fn terminal_tab_count_label(count: usize) -> String {
    format!("{count} terminal{}", if count == 1 { "" } else { "s" })
}

pub(crate) fn terminal_tab_secondary_label(pane: &Pane) -> Option<&str> {
    pane.kind
        .is_terminal()
        .then(|| pane.custom_title.is_none().then_some(pane.shell.as_str()))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::{
        FocusResync, Pane, Uuid, Workspace, WorkstationTabEntry, focus_resync_for,
        partition_workstation_entries, same_machine, terminal_tab_count_label,
        top_level_workstation, visible_workstation_tree, workspace_tab_click_target,
        workspace_tab_entries, workspace_tab_set, workspace_tab_standalone_pane,
    };
    use crate::helpers::workspace_layout_for_focused_pane;
    use crate::helpers::workspace_terminal_tabs;
    use hh_protocol::PaneLayout;
    use hh_protocol::SessionSnapshot;
    use hh_protocol::SplitAxis;
    use hh_protocol::WorkspaceConnection;
    use std::collections::HashSet;

    fn make_pane(id: u128) -> Pane {
        Pane {
            status_changed_at_ms: 0,
            id: Uuid::from_u128(id),
            kind: hh_protocol::PaneKind::Terminal,
            title: format!("Terminal {id}"),
            shell: "zsh".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        }
    }

    fn make_tab(id: u128, custom_title: Option<&str>, layout: PaneLayout) -> hh_protocol::Tab {
        hh_protocol::Tab {
            owner_thread: None,
            owner_bot: None,
            id: Uuid::from_u128(id),
            title: format!("Tab {id}"),
            custom_title: custom_title.map(str::to_owned),
            color: None,
            custom_icon: None,
            pinned: false,
            layout,
        }
    }

    fn leaf(pane: u128) -> PaneLayout {
        PaneLayout::Leaf {
            pane: make_pane(pane),
        }
    }

    fn workstation(id: u128, parent: Option<u128>, pinned: bool, order: u32) -> Workspace {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.id = Uuid::from_u128(id);
        workspace.home = false;
        workspace.parent_workstation = parent.map(Uuid::from_u128);
        workspace.pinned = pinned;
        workspace.order = order;
        workspace
    }

    #[test]
    fn sidebar_partitions_pinned_then_other_tabs() {
        fn entry(tab_id: u128, pinned: bool) -> WorkstationTabEntry<'static> {
            WorkstationTabEntry {
                tab_id: Uuid::from_u128(tab_id),
                label: None,
                color: None,
                pinned,
                panes: Vec::new(),
            }
        }
        let (pinned, rest) = partition_workstation_entries(vec![
            entry(10, false),
            entry(20, true),
            entry(30, false),
            entry(40, true),
        ]);

        assert_eq!(
            pinned.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            [Uuid::from_u128(20), Uuid::from_u128(40)]
        );
        assert_eq!(
            rest.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            [Uuid::from_u128(10), Uuid::from_u128(30)]
        );
    }

    #[test]
    fn named_and_multi_pane_tabs_precede_single_panes() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            make_tab(10, None, leaf(1)),
            make_tab(20, Some("Named"), leaf(2)),
            make_tab(
                30,
                None,
                PaneLayout::Stack {
                    panes: vec![make_pane(3), make_pane(4)],
                    active: Uuid::from_u128(3),
                },
            ),
            make_tab(
                40,
                None,
                PaneLayout::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 0.5,
                    first: Box::new(leaf(5)),
                    second: Box::new(leaf(6)),
                },
            ),
            make_tab(50, None, leaf(7)),
        ];
        let expected = [20, 30, 40, 10, 50].map(Uuid::from_u128).to_vec();

        let entries = workspace_tab_entries(&workspace);
        assert_eq!(
            entries.iter().map(|entry| entry.label).collect::<Vec<_>>(),
            vec![Some("Named"), Some("Tab 30"), Some("Tab 40"), None, None]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.panes.len())
                .collect::<Vec<_>>(),
            vec![1, 2, 2, 1, 1]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            workspace_tab_set(&workspace)
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn workstation_tree_nests_children_in_sibling_order_under_expanded_parents() {
        let mut bot = workstation(90, None, false, 0);
        bot.kind = hh_protocol::WorkspaceKind::Bot;
        let workspaces = vec![
            workstation(1, None, false, 1),
            workstation(2, None, true, 5),
            workstation(11, Some(1), false, 2),
            workstation(12, Some(1), true, 9),
            workstation(13, Some(1), false, 0),
            workstation(111, Some(11), false, 0),
            workstation(99, Some(404), false, 0),
            bot,
        ];
        let ids = |rows: Vec<(&Workspace, usize)>| {
            rows.into_iter()
                .map(|(workspace, depth)| (workspace.id.as_u128(), depth))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(visible_workstation_tree(&workspaces, &HashSet::new())),
            [(2, 1), (99, 1), (1, 1)],
            "collapsed parents hide nested workstations; orphans surface at the top level"
        );
        let expanded = [1, 11].map(Uuid::from_u128).into_iter().collect();
        assert_eq!(
            ids(visible_workstation_tree(&workspaces, &expanded)),
            [(2, 1), (99, 1), (1, 1), (12, 2), (13, 2), (11, 2), (111, 3)]
        );
        let only_child = [11].map(Uuid::from_u128).into_iter().collect();
        assert_eq!(
            ids(visible_workstation_tree(&workspaces, &only_child)),
            [(2, 1), (99, 1), (1, 1)],
            "an expanded child stays hidden under a collapsed parent"
        );
    }

    #[test]
    fn tabs_move_only_between_workstations_on_one_machine() {
        let ssh = |destination: &str, status| WorkspaceConnection::SystemSsh {
            destination: destination.to_owned(),
            status,
        };
        let connected = hh_protocol::WorkspaceConnectionStatus::Connected;
        let offline = hh_protocol::WorkspaceConnectionStatus::Offline;
        assert!(same_machine(
            &WorkspaceConnection::Local,
            &WorkspaceConnection::Local
        ));
        assert!(same_machine(
            &ssh("dev@box", connected),
            &ssh("dev@box", offline)
        ));
        assert!(!same_machine(
            &ssh("dev@box", connected),
            &ssh("dev@other", connected)
        ));
        assert!(!same_machine(
            &WorkspaceConnection::Local,
            &ssh("dev@box", connected)
        ));
    }

    #[test]
    fn top_level_workstation_climbs_to_the_root() {
        let workspaces = vec![
            workstation(1, None, false, 0),
            workstation(11, Some(1), false, 0),
            workstation(111, Some(11), false, 0),
        ];
        for id in [1, 11, 111] {
            assert_eq!(
                top_level_workstation(&workspaces, Uuid::from_u128(id)),
                Some(Uuid::from_u128(1))
            );
        }
        assert_eq!(top_level_workstation(&workspaces, Uuid::from_u128(5)), None);
    }

    #[test]
    fn strip_click_target_resolves_from_current_snapshot() {
        let tab_id = Uuid::from_u128(30);
        let focused_pane = Uuid::from_u128(1);
        let active_pane = Uuid::from_u128(2);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            make_tab(
                30,
                None,
                PaneLayout::Stack {
                    panes: vec![make_pane(1), make_pane(2)],
                    active: active_pane,
                },
            ),
            make_tab(40, None, leaf(3)),
        ];

        assert_eq!(
            workspace_tab_click_target(&workspace, tab_id, None),
            Some(active_pane)
        );
        assert_eq!(
            workspace_tab_click_target(&workspace, tab_id, Some(focused_pane)),
            Some(focused_pane)
        );
        let recovered_active_pane = Uuid::from_u128(5);
        workspace.tabs[0].layout = PaneLayout::Stack {
            panes: vec![make_pane(4), make_pane(5)],
            active: recovered_active_pane,
        };
        assert_eq!(
            workspace_tab_click_target(&workspace, tab_id, Some(focused_pane)),
            Some(recovered_active_pane)
        );
        assert_eq!(
            workspace_tab_click_target(&workspace, Uuid::from_u128(999), None),
            None
        );
    }

    #[test]
    fn only_unnamed_single_pane_tabs_render_without_a_secondary_strip() {
        let mut tab = make_tab(10, None, leaf(1));
        assert_eq!(
            workspace_tab_standalone_pane(&tab).map(|pane| pane.id),
            Some(Uuid::from_u128(1))
        );
        tab.layout = PaneLayout::Stack {
            panes: vec![make_pane(2)],
            active: Uuid::from_u128(2),
        };
        assert_eq!(
            workspace_tab_standalone_pane(&tab).map(|pane| pane.id),
            Some(Uuid::from_u128(2))
        );

        tab.custom_title = Some("Named tab".to_owned());
        assert!(workspace_tab_standalone_pane(&tab).is_none());
        tab.custom_title = None;
        tab.layout = PaneLayout::Stack {
            panes: vec![make_pane(3), make_pane(4)],
            active: Uuid::from_u128(3),
        };
        assert!(workspace_tab_standalone_pane(&tab).is_none());
    }

    #[test]
    fn inactive_inner_tab_reasserts_focus_instead_of_falling_back() {
        let active = Uuid::from_u128(1);
        let requested = Uuid::from_u128(2);

        assert_eq!(
            focus_resync_for(&[active], Some(requested), true),
            FocusResync::Reassert(requested)
        );
    }

    #[test]
    fn workspace_rail_empty_state_and_tab_count_labels_are_explicit() {
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs.clear();

        assert!(workspace_terminal_tabs(&workspace).is_empty());
        assert_eq!(terminal_tab_count_label(0), "0 terminals");
        assert_eq!(terminal_tab_count_label(1), "1 terminal");
    }

    #[test]
    fn workstation_rows_start_collapsed_but_can_expand_after_creation() {
        let workstation = SessionSnapshot::seeded().workspaces.remove(0);
        let mut expanded_workstations: HashSet<Uuid> = HashSet::new();

        assert!(!expanded_workstations.contains(&workstation.id));
        assert_eq!(
            terminal_tab_count_label(workspace_terminal_tabs(&workstation).len()),
            "1 terminal"
        );

        assert!(expanded_workstations.insert(workstation.id));
        assert!(expanded_workstations.contains(&workstation.id));
    }

    #[test]
    fn focused_workspace_tab_layout_is_rendered_instead_of_the_first_tab() {
        let pane = |id, title: &str| Pane {
            status_changed_at_ms: 0,

            id: Uuid::from_u128(id),
            kind: hh_protocol::PaneKind::Terminal,
            title: title.to_owned(),
            shell: "tmux".to_owned(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let first = pane(1, "SSH");
        let tmux = pane(2, "tmux $2");
        let workspace = Workspace {
            owner_bot: None,
            bot: None,

            id: Uuid::nil(),
            title: "Remote".to_owned(),
            color: None,
            pinned: false,
            pin_order: 0,
            order: 0,
            active_terminal_count: 2,
            connection: WorkspaceConnection::Local,
            working_dir: None,
            kind: hh_protocol::WorkspaceKind::Workstation,
            parent_workstation: None,
            home: false,
            instructions: None,
            custom_icon: None,
            tabs: vec![
                hh_protocol::Tab {
                    owner_thread: None,
                    owner_bot: None,

                    id: Uuid::from_u128(10),
                    title: "SSH".to_owned(),
                    custom_title: None,
                    color: None,
                    custom_icon: None,
                    pinned: false,
                    layout: PaneLayout::Leaf {
                        pane: first.clone(),
                    },
                },
                hh_protocol::Tab {
                    owner_thread: None,
                    owner_bot: None,

                    id: Uuid::from_u128(20),
                    title: "tmux".to_owned(),
                    custom_title: None,
                    color: None,
                    custom_icon: None,
                    pinned: false,
                    layout: PaneLayout::Leaf { pane: tmux.clone() },
                },
            ],
        };

        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(tmux.id)),
            Some(&PaneLayout::Leaf { pane: tmux })
        );
        assert_eq!(
            workspace_layout_for_focused_pane(&workspace, Some(Uuid::from_u128(99))),
            Some(&PaneLayout::Leaf { pane: first })
        );
    }
}
