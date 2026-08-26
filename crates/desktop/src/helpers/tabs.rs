use crate::helpers::{collect_terminal_tabs, find_pane, visible_panes};

use hh_protocol::{AppearanceColor, Pane, PaneLayout, Workspace};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceTabScope {
    Workstation,
    Project(Uuid),
}

pub(crate) struct WorkspaceTabSet<'a> {
    pub(crate) scope: WorkspaceTabScope,
    pub(crate) tabs: Vec<&'a hh_protocol::Tab>,
}

pub(crate) fn workspace_scope_for_tab(workspace: &Workspace, tab_id: Uuid) -> WorkspaceTabScope {
    let Some(tab) = workspace.tabs.iter().find(|tab| tab.id == tab_id) else {
        return WorkspaceTabScope::Workstation;
    };
    if tab.project_dir.is_some() {
        WorkspaceTabScope::Project(tab.id)
    } else if let Some(parent_id) = tab.parent_tab.filter(|parent_id| {
        workspace
            .tabs
            .iter()
            .any(|parent| parent.id == *parent_id && parent.project_dir.is_some())
    }) {
        WorkspaceTabScope::Project(parent_id)
    } else {
        WorkspaceTabScope::Workstation
    }
}
fn workspace_tab_is_root(workspace: &Workspace, tab: &hh_protocol::Tab) -> bool {
    tab.parent_tab.is_none()
        || !workspace
            .tabs
            .iter()
            .any(|candidate| Some(candidate.id) == tab.parent_tab)
}

/// Tabs shown in the persistent strip above the viewport.
///
/// A workstation displays root tabs as projects, groups, then loose panes,
/// preserving insertion order within each category. Project scope is an explicit
/// drill-down containing that project root and its direct children.
pub(crate) fn workspace_tab_set(
    workspace: &Workspace,
    requested_scope: WorkspaceTabScope,
) -> WorkspaceTabSet<'_> {
    let scope = match requested_scope {
        WorkspaceTabScope::Project(project_id)
            if workspace
                .tabs
                .iter()
                .any(|tab| tab.id == project_id && tab.project_dir.is_some()) =>
        {
            requested_scope
        }
        WorkspaceTabScope::Workstation | WorkspaceTabScope::Project(_) => {
            WorkspaceTabScope::Workstation
        }
    };
    let tabs = match scope {
        WorkspaceTabScope::Workstation => {
            let mut tabs = workspace
                .tabs
                .iter()
                .filter(|tab| workspace_tab_is_root(workspace, tab))
                .collect::<Vec<_>>();
            tabs.sort_by_key(|tab| workspace_tab_rank(tab));
            tabs
        }
        WorkspaceTabScope::Project(project_id) => workspace
            .tabs
            .iter()
            .filter(|tab| tab.id == project_id)
            .chain(
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab.parent_tab == Some(project_id)),
            )
            .collect(),
    };
    WorkspaceTabSet { scope, tabs }
}

/// Collapsible sidebar sections rendered inside one workstation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SidebarSection {
    Pinned,
    Projects,
}

impl SidebarSection {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::Projects => "Projects",
        }
    }

    pub(crate) fn element_id(self) -> &'static str {
        match self {
            Self::Pinned => "sidebar-pinned-section",
            Self::Projects => "sidebar-projects-section",
        }
    }
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

/// Strip tab that should render as active for the focused pane.
///
/// Workstation scope maps a pane inside a project's child tab to the project
/// root; project scope highlights the child tab itself.
pub(crate) fn workspace_strip_active_tab(
    workspace: &Workspace,
    scope: WorkspaceTabScope,
    focused_pane: Option<Uuid>,
) -> Option<Uuid> {
    let pane_id = focused_pane?;
    let tab = workspace
        .tabs
        .iter()
        .find(|tab| find_pane(&tab.layout, pane_id).is_some())?;
    match scope {
        WorkspaceTabScope::Workstation => Some(
            tab.parent_tab
                .filter(|parent| {
                    workspace
                        .tabs
                        .iter()
                        .any(|candidate| candidate.id == *parent)
                })
                .unwrap_or(tab.id),
        ),
        WorkspaceTabScope::Project(_) => Some(tab.id),
    }
}

pub(crate) fn workspace_tab_standalone_pane(tab: &hh_protocol::Tab) -> Option<&Pane> {
    if tab.project_dir.is_some() || tab.parent_tab.is_some() || tab.custom_title.is_some() {
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

/// Root-tab display rank: projects first, then groups, then loose panes.
pub(crate) fn workspace_tab_rank(tab: &hh_protocol::Tab) -> u8 {
    if tab.project_dir.is_some() {
        0
    } else if tab.custom_title.is_some() || pane_count(&tab.layout) != 1 {
        1
    } else {
        2
    }
}

/// One sidebar entry per tab. `group_label` is `Some` exactly when the tab
/// must render as a group: it holds several terminals, or the user named it.
pub(crate) struct WorkstationTabEntry<'a> {
    pub(crate) tab_id: Uuid,
    pub(crate) group_label: Option<String>,
    pub(crate) project_dir: Option<String>,
    pub(crate) color: Option<AppearanceColor>,
    pub(crate) custom_icon: Option<String>,
    pub(crate) pinned: bool,
    pub(crate) panes: Vec<&'a Pane>,
    pub(crate) children: Vec<WorkstationTabEntry<'a>>,
}

pub(crate) fn workspace_tab_entries(workspace: &Workspace) -> Vec<WorkstationTabEntry<'_>> {
    fn make_entry(tab: &hh_protocol::Tab) -> WorkstationTabEntry<'_> {
        let mut panes = Vec::new();
        collect_terminal_tabs(&tab.layout, &mut panes);
        let group_label = (panes.len() >= 2
            || tab.custom_title.is_some()
            || tab.project_dir.is_some())
        .then(|| {
            tab.custom_title
                .clone()
                .unwrap_or_else(|| tab.title.clone())
        });
        WorkstationTabEntry {
            tab_id: tab.id,
            group_label,
            project_dir: tab.project_dir.clone(),
            color: tab.color,
            custom_icon: tab.custom_icon.clone(),
            pinned: tab.pinned,
            panes,
            children: Vec::new(),
        }
    }
    let mut root_tabs = workspace
        .tabs
        .iter()
        .filter(|tab| workspace_tab_is_root(workspace, tab))
        .collect::<Vec<_>>();
    root_tabs.sort_by_key(|tab| workspace_tab_rank(tab));
    let mut roots = root_tabs.into_iter().map(make_entry).collect::<Vec<_>>();
    for tab in workspace.tabs.iter().filter(|tab| {
        tab.parent_tab.is_some()
            && workspace
                .tabs
                .iter()
                .any(|candidate| Some(candidate.id) == tab.parent_tab)
    }) {
        if let Some(parent) = roots
            .iter_mut()
            .find(|entry| Some(entry.tab_id) == tab.parent_tab)
        {
            parent.children.push(make_entry(tab));
        }
    }
    roots
}

/// Sidebar display partitions inside one workstation: pinned roots,
/// unpinned projects, then unpinned free-floating tabs; relative order kept.
pub(crate) fn partition_workstation_entries(
    entries: Vec<WorkstationTabEntry<'_>>,
) -> (
    Vec<WorkstationTabEntry<'_>>,
    Vec<WorkstationTabEntry<'_>>,
    Vec<WorkstationTabEntry<'_>>,
) {
    let (pinned, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(|entry| entry.pinned);
    let (projects, floating): (Vec<_>, Vec<_>) = rest
        .into_iter()
        .partition(|entry| entry.project_dir.is_some());
    (pinned, projects, floating)
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
        FocusResync, Pane, Uuid, Workspace, WorkspaceTabScope, WorkstationTabEntry,
        focus_resync_for, partition_workstation_entries, terminal_tab_count_label,
        workspace_scope_for_tab, workspace_strip_active_tab, workspace_tab_click_target,
        workspace_tab_entries, workspace_tab_focus_target, workspace_tab_set,
        workspace_tab_standalone_pane,
    };
    use crate::helpers::workspace_layout_for_focused_pane;
    use crate::helpers::workspace_terminal_tabs;
    use hh_protocol::PaneLayout;
    use hh_protocol::SessionSnapshot;
    use hh_protocol::SplitAxis;
    use hh_protocol::WorkspaceConnection;
    use std::collections::HashSet;

    #[test]
    fn sidebar_partitions_pinned_then_projects_then_floating() {
        let entry = |tab_id: u128, project_dir: Option<&str>, pinned: bool| WorkstationTabEntry {
            tab_id: Uuid::from_u128(tab_id),
            group_label: None,
            project_dir: project_dir.map(str::to_owned),
            color: None,
            custom_icon: None,
            pinned,
            panes: Vec::new(),
            children: Vec::new(),
        };
        let (pinned, projects, floating) = partition_workstation_entries(vec![
            entry(10, Some("/tmp/project-a"), false),
            entry(20, None, true),
            entry(30, None, false),
            entry(40, Some("/tmp/project-d"), true),
            entry(50, None, true),
        ]);

        assert_eq!(
            pinned.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            [
                Uuid::from_u128(20),
                Uuid::from_u128(40),
                Uuid::from_u128(50)
            ]
        );
        assert_eq!(
            projects
                .iter()
                .map(|entry| entry.tab_id)
                .collect::<Vec<_>>(),
            [Uuid::from_u128(10)]
        );
        assert_eq!(
            floating
                .iter()
                .map(|entry| entry.tab_id)
                .collect::<Vec<_>>(),
            [Uuid::from_u128(30)]
        );
    }

    #[test]
    fn workspace_tab_projection_orders_groups_before_loose_tabs() {
        let make_pane = |id: u128| Pane {
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
        };
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            hh_protocol::Tab {
                id: Uuid::from_u128(10),
                title: "Single".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane: make_pane(1) },
            },
            hh_protocol::Tab {
                id: Uuid::from_u128(20),
                title: "Named".to_owned(),
                custom_title: Some("Group 1".to_owned()),
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane: make_pane(2) },
            },
            hh_protocol::Tab {
                id: Uuid::from_u128(30),
                title: "Stacked".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Stack {
                    panes: vec![make_pane(3), make_pane(4)],
                    active: Uuid::from_u128(3),
                },
            },
            hh_protocol::Tab {
                id: Uuid::from_u128(40),
                title: "Split".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 0.5,
                    first: Box::new(PaneLayout::Leaf { pane: make_pane(5) }),
                    second: Box::new(PaneLayout::Leaf { pane: make_pane(6) }),
                },
            },
        ];

        let entries = workspace_tab_entries(&workspace);

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.group_label.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("Group 1"), Some("Stacked"), Some("Split"), None]
        );
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.panes.len())
                .collect::<Vec<_>>(),
            vec![1, 2, 2, 1]
        );
        assert_eq!(
            entries.iter().map(|entry| entry.tab_id).collect::<Vec<_>>(),
            vec![
                Uuid::from_u128(20),
                Uuid::from_u128(30),
                Uuid::from_u128(40),
                Uuid::from_u128(10)
            ]
        );
    }

    #[test]
    fn workstation_strip_orders_projects_then_groups_then_loose_tabs() {
        let make_pane = |id: u128| Pane {
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
        };
        let make_leaf_tab =
            |tab_id: u128, pane_id: u128, project_dir: Option<&str>| hh_protocol::Tab {
                id: Uuid::from_u128(tab_id),
                title: format!("Tab {tab_id}"),
                custom_title: None,
                project_dir: project_dir.map(str::to_owned),
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf {
                    pane: make_pane(pane_id),
                },
            };
        let first_loose_id = Uuid::from_u128(10);
        let group_id = Uuid::from_u128(20);
        let project_id = Uuid::from_u128(30);
        let last_loose_id = Uuid::from_u128(40);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            make_leaf_tab(10, 1, None),
            hh_protocol::Tab {
                id: group_id,
                title: "Group".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Stack {
                    panes: vec![make_pane(2), make_pane(3)],
                    active: Uuid::from_u128(2),
                },
            },
            make_leaf_tab(30, 4, Some("/tmp/project")),
            make_leaf_tab(40, 5, None),
        ];
        let expected = vec![project_id, group_id, first_loose_id, last_loose_id];

        assert_eq!(
            workspace_tab_set(&workspace, WorkspaceTabScope::Workstation)
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            workspace_tab_entries(&workspace)
                .iter()
                .map(|entry| entry.tab_id)
                .collect::<Vec<_>>(),
            expected
        );

        workspace.tabs[0].parent_tab = Some(Uuid::from_u128(999));
        assert_eq!(
            workspace_tab_set(&workspace, WorkspaceTabScope::Workstation)
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn strip_click_target_resolves_from_current_snapshot() {
        let make_pane = |id: u128| Pane {
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
        };
        let group_id = Uuid::from_u128(30);
        let focused_group_pane = Uuid::from_u128(1);
        let active_group_pane = Uuid::from_u128(2);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            hh_protocol::Tab {
                id: group_id,
                title: "Group".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Stack {
                    panes: vec![make_pane(1), make_pane(2)],
                    active: active_group_pane,
                },
            },
            hh_protocol::Tab {
                id: Uuid::from_u128(40),
                title: "Loose".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane: make_pane(3) },
            },
        ];

        assert_eq!(
            workspace_tab_click_target(&workspace, group_id, None),
            Some(active_group_pane)
        );
        assert_eq!(
            workspace_tab_click_target(&workspace, group_id, Some(focused_group_pane)),
            Some(focused_group_pane)
        );
        let recovered_active_pane = Uuid::from_u128(5);
        workspace.tabs[0].layout = PaneLayout::Stack {
            panes: vec![make_pane(4), make_pane(5)],
            active: recovered_active_pane,
        };
        assert_eq!(
            workspace_tab_click_target(&workspace, group_id, Some(focused_group_pane)),
            Some(recovered_active_pane)
        );
        assert_eq!(
            workspace_tab_click_target(&workspace, Uuid::from_u128(999), None),
            None
        );
    }

    #[test]
    fn strip_active_tab_maps_project_children_to_the_project_root() {
        let make_pane = |id: u128| Pane {
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
        };
        let make_tab =
            |tab_id: u128, pane_id: u128, project_dir: Option<&str>, parent_tab: Option<Uuid>| {
                hh_protocol::Tab {
                    id: Uuid::from_u128(tab_id),
                    title: format!("Tab {tab_id}"),
                    custom_title: None,
                    project_dir: project_dir.map(str::to_owned),
                    color: None,
                    custom_icon: None,
                    parent_tab,
                    pinned: false,
                    layout: PaneLayout::Leaf {
                        pane: make_pane(pane_id),
                    },
                }
            };
        let loose_id = Uuid::from_u128(10);
        let project_id = Uuid::from_u128(30);
        let child_id = Uuid::from_u128(40);
        let child_pane_id = Uuid::from_u128(4);
        let loose_pane_id = Uuid::from_u128(1);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            make_tab(30, 3, Some("/tmp/project"), None),
            make_tab(40, 4, None, Some(project_id)),
            make_tab(10, 1, None, None),
        ];

        assert_eq!(
            workspace_strip_active_tab(
                &workspace,
                WorkspaceTabScope::Workstation,
                Some(child_pane_id)
            ),
            Some(project_id)
        );
        assert_eq!(
            workspace_strip_active_tab(
                &workspace,
                WorkspaceTabScope::Project(project_id),
                Some(child_pane_id)
            ),
            Some(child_id)
        );
        assert_eq!(
            workspace_strip_active_tab(
                &workspace,
                WorkspaceTabScope::Workstation,
                Some(loose_pane_id)
            ),
            Some(loose_id)
        );
        assert_eq!(
            workspace_strip_active_tab(&workspace, WorkspaceTabScope::Workstation, None),
            None
        );
    }

    #[test]
    fn viewport_tab_strip_keeps_explicit_workstation_and_project_scopes() {
        let make_pane = |id: u128| Pane {
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
        };
        let make_tab =
            |tab_id: u128, pane_id: u128, project_dir: Option<&str>, parent_tab: Option<Uuid>| {
                hh_protocol::Tab {
                    id: Uuid::from_u128(tab_id),
                    title: format!("Tab {tab_id}"),
                    custom_title: None,
                    project_dir: project_dir.map(str::to_owned),
                    color: None,
                    custom_icon: None,
                    parent_tab,
                    pinned: false,
                    layout: PaneLayout::Leaf {
                        pane: make_pane(pane_id),
                    },
                }
            };
        let project_id = Uuid::from_u128(30);
        let other_project_id = Uuid::from_u128(50);
        let mut workspace = SessionSnapshot::seeded().workspaces.remove(0);
        workspace.tabs = vec![
            make_tab(10, 1, None, None),
            make_tab(20, 2, None, None),
            make_tab(30, 3, Some("/tmp/project-a"), None),
            make_tab(40, 4, None, Some(project_id)),
            make_tab(50, 5, Some("/tmp/project-b"), None),
            make_tab(60, 6, None, Some(other_project_id)),
        ];

        let workstation = workspace_tab_set(&workspace, WorkspaceTabScope::Workstation);
        assert_eq!(workstation.scope, WorkspaceTabScope::Workstation);
        assert_eq!(
            workstation
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![
                project_id,
                other_project_id,
                Uuid::from_u128(10),
                Uuid::from_u128(20)
            ]
        );
        assert_eq!(
            workspace_scope_for_tab(&workspace, Uuid::from_u128(20)),
            WorkspaceTabScope::Workstation
        );
        for tab_id in [project_id, Uuid::from_u128(40)] {
            assert_eq!(
                workspace_scope_for_tab(&workspace, tab_id),
                WorkspaceTabScope::Project(project_id)
            );
        }

        let project = workspace_tab_set(&workspace, WorkspaceTabScope::Project(project_id));
        assert_eq!(project.scope, WorkspaceTabScope::Project(project_id));
        assert_eq!(
            project.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
            vec![project_id, Uuid::from_u128(40)]
        );
        let fallback =
            workspace_tab_set(&workspace, WorkspaceTabScope::Project(Uuid::from_u128(999)));
        assert_eq!(fallback.scope, WorkspaceTabScope::Workstation);
        assert_eq!(
            workspace_tab_focus_target(&workspace.tabs[3], Some(Uuid::from_u128(4))),
            Some(Uuid::from_u128(4))
        );
    }

    #[test]
    fn only_unnamed_single_pane_tabs_render_without_a_secondary_strip() {
        let make_pane = |id: u128| Pane {
            id: Uuid::from_u128(id),
            kind: hh_protocol::PaneKind::Browser {
                url: "https://example.com".to_owned(),
            },
            title: format!("Pane {id}"),
            shell: String::new(),
            color: None,
            identity: hh_protocol::TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        let mut tab = hh_protocol::Tab {
            id: Uuid::from_u128(10),
            title: "Example".to_owned(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            layout: PaneLayout::Leaf { pane: make_pane(1) },
        };

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

        tab.custom_title = Some("Named group".to_owned());
        assert!(workspace_tab_standalone_pane(&tab).is_none());
        tab.custom_title = None;
        tab.project_dir = Some("/tmp/project".to_owned());
        assert!(workspace_tab_standalone_pane(&tab).is_none());
        tab.project_dir = None;
        tab.parent_tab = Some(Uuid::from_u128(99));
        assert!(workspace_tab_standalone_pane(&tab).is_none());
        tab.parent_tab = None;
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
            id: Uuid::nil(),
            title: "Remote".to_owned(),
            color: None,
            pinned: false,
            pin_order: 0,
            order: 0,
            active_terminal_count: 2,
            connection: WorkspaceConnection::Local,
            working_dir: None,
            tabs: vec![
                hh_protocol::Tab {
                    id: Uuid::from_u128(10),
                    title: "SSH".to_owned(),
                    custom_title: None,
                    project_dir: None,
                    color: None,
                    custom_icon: None,
                    parent_tab: None,
                    pinned: false,
                    layout: PaneLayout::Leaf {
                        pane: first.clone(),
                    },
                },
                hh_protocol::Tab {
                    id: Uuid::from_u128(20),
                    title: "tmux".to_owned(),
                    custom_title: None,
                    project_dir: None,
                    color: None,
                    custom_icon: None,
                    parent_tab: None,
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
