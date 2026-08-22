//! Tab operations: layout moves, tab metadata, and reorder within workstations.
use super::{
    RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry, TerminalRuntimePane,
    encode_desired_state,
};
use crate::layout::{
    activate_tab, add_tab, collect_pane_ids, detach_pane, first_layout_pane, layout_contains,
    move_workspace_pane_to_split, move_workspace_pane_to_tab, split_lone_layout_with_replacement,
    swap_pane_ids,
};
use crate::persistence;
use crate::persistence::{MAX_TABS_PER_WORKSPACE, validate_title};
use crate::process::local_spawn_dir;
use crate::pty::PtySession;
use crate::registry::workspaces::remember_recent_color;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, DropPlacement, MAX_PANES, PaneLayout, Tab, Workspace, validate_workspace_dir,
};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

fn repair_removed_tab_children(
    workspace: &mut Workspace,
    removed_tab: Uuid,
    replacement_parent: Option<Uuid>,
) {
    let replacement_parent = replacement_parent.filter(|replacement| {
        *replacement != removed_tab
            && workspace.tabs.iter().any(|tab| {
                tab.id == *replacement && tab.parent_tab.is_none() && tab.project_dir.is_some()
            })
    });
    for tab in &mut workspace.tabs {
        if tab.parent_tab == Some(removed_tab) {
            tab.parent_tab = replacement_parent;
        }
    }
}

impl SessionRegistry {
    /// Appends one more top-level tab to a workstation that already has a layout.
    /// Unlike `create_workspace_terminal` this is deliberately not idempotent:
    /// every request adds a tab, which is what the workstation menu's "New Tab"
    /// means.
    pub fn create_workspace_tab(&self, workspace_id: Uuid) -> Result<Uuid> {
        self.append_workspace_tab(workspace_id, None, None, None)
    }

    /// Appends a named group holding its first terminal, so the group is visible
    /// and right-clickable before a second terminal exists.
    pub fn create_workspace_group(
        &self,
        workspace_id: Uuid,
        parent_tab: Option<Uuid>,
    ) -> Result<Uuid> {
        let number = {
            let mut state = self.state.write();
            let number = state.next_group_number;
            state.next_group_number = state.next_group_number.saturating_add(1);
            number
        };
        self.append_workspace_tab(
            workspace_id,
            Some(format!("Group {number}")),
            None,
            parent_tab,
        )
    }

    pub fn create_workspace_project(
        &self,
        workspace_id: Uuid,
        working_dir: &str,
        title: Option<&str>,
    ) -> Result<Uuid> {
        validate_workspace_dir(working_dir).map_err(anyhow::Error::from)?;
        let title = title.map_or_else(
            || {
                working_dir
                    .rsplit('/')
                    .find(|component| !component.is_empty())
                    .unwrap_or("Project")
                    .to_owned()
            },
            str::to_owned,
        );
        self.append_workspace_tab(
            workspace_id,
            Some(title),
            Some(working_dir.to_owned()),
            None,
        )
    }

    pub(crate) fn workspace_tab_dir_override(
        &self,
        workspace_id: Uuid,
        project_dir: Option<&str>,
        parent_tab: Option<Uuid>,
    ) -> Result<Option<String>> {
        let state = self.state.read();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        let parent_project_dir = if let Some(parent_id) = parent_tab {
            if project_dir.is_some() {
                bail!("parent tab {parent_id} must be a project in the same workstation");
            }
            let parent = workspace
                .tabs
                .iter()
                .find(|tab| tab.id == parent_id)
                .filter(|tab| tab.parent_tab.is_none() && tab.project_dir.is_some())
                .with_context(|| {
                    format!("parent tab {parent_id} must be a project in the same workstation")
                })?;
            parent.project_dir.clone()
        } else {
            None
        };
        Ok(project_dir
            .map(str::to_owned)
            .or(parent_project_dir)
            .or_else(|| workspace.working_dir.clone()))
    }

    pub(crate) fn append_workspace_tab(
        &self,
        workspace_id: Uuid,
        custom_title: Option<String>,
        project_dir: Option<String>,
        parent_tab: Option<Uuid>,
    ) -> Result<Uuid> {
        let dir_override =
            self.workspace_tab_dir_override(workspace_id, project_dir.as_deref(), parent_tab)?;

        let pane_id = Uuid::new_v4();
        let cwd = local_spawn_dir(dir_override.as_deref())?;
        let (session, kind) =
            self.spawn_pane_for_workspace(pane_id, workspace_id, &cwd, dir_override.as_deref())?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let workspace_index = state
                .snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if state.snapshot.workspaces[workspace_index].tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
            }
            let mut pane = state.new_pane(pane_id, Some(cwd.as_path()));
            if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
                "ssh".clone_into(&mut pane.shell);
            }
            let tab = Tab {
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title,
                project_dir,
                color: None,
                custom_icon: None,
                parent_tab,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            };
            let workspace = &mut state.snapshot.workspaces[workspace_index];
            let insertion_index = if let Some(parent_id) = parent_tab {
                let parent_index = workspace
                    .tabs
                    .iter()
                    .position(|candidate| {
                        candidate.id == parent_id
                            && candidate.parent_tab.is_none()
                            && candidate.project_dir.is_some()
                    })
                    .with_context(|| {
                        format!("parent tab {parent_id} must be a project in the same workstation")
                    })?;
                workspace
                    .tabs
                    .iter()
                    .enumerate()
                    .filter_map(|(index, candidate)| {
                        (candidate.parent_tab == Some(parent_id)).then_some(index)
                    })
                    .next_back()
                    .unwrap_or(parent_index)
                    + 1
            } else {
                workspace.tabs.len()
            };
            workspace.tabs.insert(insertion_index, tab);
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn activate_tab(&self, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let did_activate = state.snapshot.workspaces.iter_mut().any(|workspace| {
            workspace
                .tabs
                .iter_mut()
                .any(|tab| activate_tab(&mut tab.layout, pane_id))
        });
        if !did_activate {
            bail!("pane tab {pane_id} does not exist");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn swap_panes(&self, source_pane: Uuid, target_pane: Uuid) -> Result<()> {
        if source_pane == target_pane {
            return Ok(());
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be rearranged");
        }
        let mut did_swap = false;
        for workspace in &mut state.snapshot.workspaces {
            for tab in &mut workspace.tabs {
                if layout_contains(&tab.layout, source_pane)
                    && layout_contains(&tab.layout, target_pane)
                {
                    swap_pane_ids(&mut tab.layout, source_pane, target_pane);
                    did_swap = true;
                    break;
                }
            }
            if did_swap {
                break;
            }
        }
        if !did_swap {
            bail!("panes can only be rearranged inside the same workstation layout");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn move_pane_to_split(
        &self,
        source_pane: Uuid,
        target_pane: Uuid,
        placement: DropPlacement,
    ) -> Result<()> {
        if source_pane == target_pane {
            return self.split_lone_pane_with_replacement(source_pane, placement);
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be rearranged");
        }
        let did_move = state.snapshot.workspaces.iter_mut().any(|workspace| {
            move_workspace_pane_to_split(workspace, source_pane, target_pane, placement)
        });
        if !did_move {
            bail!("source and target panes must exist in the same workstation");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub(crate) fn split_lone_pane_with_replacement(
        &self,
        pane_id: Uuid,
        placement: DropPlacement,
    ) -> Result<()> {
        let replacement_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(pane_id)?;
        let workspace_id = self.workspace_for_pane(pane_id)?;
        let replacement_session =
            PtySession::spawn_local(replacement_id, workspace_id, &cwd, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let replacement = state.new_pane(replacement_id, Some(cwd.as_path()));
            let did_split = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace.tabs.iter_mut().any(|tab| {
                    split_lone_layout_with_replacement(
                        &mut tab.layout,
                        pane_id,
                        replacement.clone(),
                        placement,
                    )
                })
            });
            if !did_split {
                bail!("a self-directed drop requires a pane containing exactly one terminal");
            }
            state.panes.insert(
                replacement_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&replacement_session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                    }),
                },
            );
            state.snapshot.revision += 1;
            {
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            };
            Ok(())
        })();
        if result.is_err() {
            let _ = replacement_session.terminate_and_wait();
        }
        result
    }

    pub fn move_pane_to_tab(&self, source_pane: Uuid, target_pane: Uuid) -> Result<()> {
        if source_pane == target_pane {
            return self.activate_tab(source_pane);
        }
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) || !state.panes.contains_key(&target_pane) {
            bail!("both panes must exist before they can be merged");
        }
        let did_move = state
            .snapshot
            .workspaces
            .iter_mut()
            .any(|workspace| move_workspace_pane_to_tab(workspace, source_pane, target_pane));
        if !did_move {
            bail!("source and target panes must exist in the same workstation");
        }
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn move_pane_to_group(&self, source_pane: Uuid, target_tab: Uuid) -> Result<()> {
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) {
            bail!("source pane {source_pane} does not exist");
        }
        let source_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| layout_contains(&tab.layout, source_pane))
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("source pane {source_pane} does not belong to a tab"))?;
        let target_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == target_tab)
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("target group {target_tab} does not exist"))?;
        if source_location.0 != target_location.0 {
            bail!("panes can only move between groups in the same workstation");
        }
        if source_location == target_location {
            return Ok(());
        }

        let workspace = &mut state.snapshot.workspaces[source_location.0];
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        let mut target_layout = workspace.tabs[target_location.1].layout.clone();
        let target_pane = first_layout_pane(&target_layout);
        if !add_tab(&mut target_layout, target_pane, pane) {
            bail!("target group {target_tab} cannot accept pane {source_pane}");
        }
        workspace.tabs[target_location.1].layout = target_layout;
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            let removed_tab = workspace.tabs[source_location.1].id;
            repair_removed_tab_children(workspace, removed_tab, Some(target_tab));
            workspace.tabs.remove(source_location.1);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub(crate) fn resolve_move_parent(
        workspace: &Workspace,
        target_index: usize,
        parent_tab: Option<Uuid>,
    ) -> Result<Option<Uuid>> {
        match parent_tab {
            Some(parent) => {
                let valid = workspace.tabs.iter().any(|tab| {
                    tab.id == parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
                });
                if !valid {
                    bail!("parent tab {parent} must be a project in the same workstation");
                }
                Ok(Some(parent))
            }
            None => Ok(workspace.tabs[target_index].parent_tab.filter(|parent| {
                workspace.tabs.iter().any(|tab| {
                    tab.id == *parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
                })
            })),
        }
    }

    pub fn move_pane_to_new_tab(
        &self,
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
        parent_tab: Option<Uuid>,
    ) -> Result<()> {
        let mut state = self.state.write();
        if !state.panes.contains_key(&source_pane) {
            bail!("source pane {source_pane} does not exist");
        }
        let source_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| layout_contains(&tab.layout, source_pane))
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("source pane {source_pane} does not belong to a tab"))?;
        let target_location = state
            .snapshot
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_index, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .position(|tab| tab.id == target_tab)
                    .map(|tab_index| (workspace_index, tab_index))
            })
            .with_context(|| format!("target tab {target_tab} does not exist"))?;
        if source_location.0 != target_location.0 {
            bail!("panes can only move between tabs in the same workstation");
        }

        let workspace = &mut state.snapshot.workspaces[source_location.0];
        let append_to_project = parent_tab == Some(target_tab);
        let mut resolved_parent =
            Self::resolve_move_parent(workspace, target_location.1, parent_tab)?;
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        if remaining.is_none() && workspace.tabs[source_location.1].id == target_tab {
            return Ok(());
        }
        if remaining.is_none() && resolved_parent == Some(workspace.tabs[source_location.1].id) {
            resolved_parent = None;
        }
        if remaining.is_some() && workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            let removed_tab = workspace.tabs[source_location.1].id;
            repair_removed_tab_children(workspace, removed_tab, resolved_parent);
            workspace.tabs.remove(source_location.1);
        }
        let target_index = workspace
            .tabs
            .iter()
            .position(|tab| tab.id == target_tab)
            .with_context(|| format!("target tab {target_tab} disappeared during the move"))?;
        let insertion_index = if append_to_project {
            workspace
                .tabs
                .iter()
                .enumerate()
                .filter_map(|(index, candidate)| {
                    (candidate.parent_tab == Some(target_tab)).then_some(index)
                })
                .next_back()
                .unwrap_or(target_index)
                + 1
        } else {
            target_index + usize::from(after)
        };
        workspace.tabs.insert(
            insertion_index,
            Tab {
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: resolved_parent,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub fn rename_tab(&self, tab_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        validate_title(title, "group")?;
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("group {tab_id} does not exist"))?;
        tab.custom_title = Some(title.to_owned());
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub fn set_tab_custom_icon(&self, tab_id: Uuid, icon: Option<String>) -> Result<()> {
        if let Some(icon) = icon.as_deref() {
            persistence::validate_custom_icon_id(icon)?;
        }
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        tab.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn close_tab(&self, tab_id: Uuid) -> Result<()> {
        let (sessions, bytes) = {
            let mut state = self.state.write();
            let workspace_index = state
                .snapshot
                .workspaces
                .iter()
                .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
                .with_context(|| format!("tab {tab_id} does not exist"))?;
            let (tab_ids, pane_ids) = {
                let workspace = &state.snapshot.workspaces[workspace_index];
                let mut tab_ids = HashSet::from([tab_id]);
                loop {
                    let previous_len = tab_ids.len();
                    let children = workspace
                        .tabs
                        .iter()
                        .filter_map(|tab| {
                            tab.parent_tab
                                .is_some_and(|parent| tab_ids.contains(&parent))
                                .then_some(tab.id)
                        })
                        .collect::<Vec<_>>();
                    tab_ids.extend(children);
                    if tab_ids.len() == previous_len {
                        break;
                    }
                }
                let mut pane_ids = Vec::new();
                for tab in workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab_ids.contains(&tab.id))
                {
                    collect_pane_ids(&tab.layout, &mut pane_ids);
                }
                (tab_ids, pane_ids)
            };
            let sessions = pane_ids
                .iter()
                .filter_map(|pane_id| {
                    state
                        .panes
                        .get(pane_id)?
                        .terminal()
                        .map(|terminal| Arc::clone(&terminal.session))
                })
                .collect::<Vec<_>>();
            let terminal_count = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
            let workspace = &mut state.snapshot.workspaces[workspace_index];
            workspace.tabs.retain(|tab| !tab_ids.contains(&tab.id));
            workspace.active_terminal_count = workspace
                .active_terminal_count
                .saturating_sub(terminal_count);
            for pane_id in pane_ids {
                state.panes.remove(&pane_id);
            }
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            let bytes = encode_desired_state(&state)?;
            (sessions, bytes)
        };

        let mut termination_errors = Vec::new();
        for session in sessions {
            if let Err(error) = session.terminate_and_wait() {
                termination_errors.push(format!("{error:#}"));
            }
        }
        self.write_snapshot(&bytes)?;
        if !termination_errors.is_empty() {
            bail!(
                "tab {tab_id} closed, but session termination failed: {}",
                termination_errors.join("; ")
            );
        }
        Ok(())
    }

    pub fn set_tab_color(&self, tab_id: Uuid, color: Option<AppearanceColor>) -> Result<()> {
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        tab.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_tab_working_dir(&self, tab_id: Uuid, working_dir: String) -> Result<()> {
        validate_workspace_dir(&working_dir).map_err(anyhow::Error::from)?;
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .find_map(|workspace| workspace.tabs.iter_mut().find(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let project_dir = tab
            .project_dir
            .as_mut()
            .with_context(|| format!("tab {tab_id} is not a project"))?;
        if *project_dir == working_dir {
            return Ok(());
        }
        *project_dir = working_dir;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn reorder_tab(&self, tab_id: Uuid, target_tab_id: Uuid, after: bool) -> Result<()> {
        let mut state = self.state.write();
        let source_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let target_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == target_tab_id))
            .with_context(|| format!("target tab {target_tab_id} does not exist"))?;
        if source_workspace != target_workspace {
            bail!("tabs can only be reordered within the same workstation");
        }
        if tab_id == target_tab_id {
            return Ok(());
        }
        let tabs = &mut state.snapshot.workspaces[source_workspace].tabs;
        let source = tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .context("source tab disappeared while reordering")?;
        let target = tabs
            .iter()
            .position(|tab| tab.id == target_tab_id)
            .context("target tab disappeared while reordering")?;
        let target_parent = tabs[target].parent_tab.filter(|parent| {
            tabs.iter().any(|tab| {
                tab.id == *parent && tab.parent_tab.is_none() && tab.project_dir.is_some()
            })
        });
        let mut tab = tabs.remove(source);
        tab.parent_tab = if tab.project_dir.is_some() {
            None
        } else {
            target_parent
        };
        let target = tabs
            .iter()
            .position(|candidate| candidate.id == target_tab_id)
            .context("target tab disappeared while reordering")?;
        tabs.insert(target + usize::from(after), tab);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn move_tab_to_project(&self, tab_id: Uuid, project_tab: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let source_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        let project_workspace = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.tabs.iter().any(|tab| tab.id == project_tab))
            .with_context(|| format!("project {project_tab} does not exist"))?;
        if source_workspace != project_workspace {
            bail!("tabs can only move within the same workstation");
        }

        let tabs = &mut state.snapshot.workspaces[source_workspace].tabs;
        let source_index = tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .context("source tab disappeared while moving to a project")?;
        let project_index = tabs
            .iter()
            .position(|tab| tab.id == project_tab)
            .context("project tab disappeared while moving a tab")?;
        if tabs[project_index].project_dir.is_none() || tabs[project_index].parent_tab.is_some() {
            bail!("target {project_tab} is not a project");
        }
        if tabs[source_index].project_dir.is_some() {
            bail!("a project cannot nest inside another project");
        }
        if tabs[source_index].parent_tab == Some(project_tab) {
            return Ok(());
        }

        let mut tab = tabs.remove(source_index);
        tab.parent_tab = Some(project_tab);
        let project_index = tabs
            .iter()
            .position(|candidate| candidate.id == project_tab)
            .context("project tab disappeared while moving a tab")?;
        let insertion_index = tabs
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (candidate.parent_tab == Some(project_tab)).then_some(index)
            })
            .next_back()
            .unwrap_or(project_index)
            + 1;
        tabs.insert(insertion_index, tab);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_tab_pinned(&self, tab_id: Uuid, pinned: bool) -> Result<()> {
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
        if tab.pinned == pinned {
            return Ok(());
        }
        tab.pinned = pinned;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{first_pane_id, tab_id_for_pane};
    use crate::registry::{SessionRegistry, create_owner_only_directory};
    use hh_protocol::{PaneKind, SplitAxis};
    use uuid::Uuid;

    #[test]
    fn group_names_are_validated_and_survive_restart() {
        let directory = std::env::temp_dir().join(format!("hh-group-name-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;

        let pane_id = registry.create_workspace_group(workspace_id, None).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, pane_id))
            .expect("new group owns the returned pane");
        let tab_id = tab.id;
        assert_eq!(tab.custom_title.as_deref(), Some("Group 1"));

        registry.rename_tab(tab_id, "  Design bank  ").unwrap();
        assert!(registry.rename_tab(tab_id, "").is_err());
        assert!(registry.rename_tab(tab_id, &"x".repeat(81)).is_err());
        let renamed = registry.snapshot().unwrap();
        let tab = renamed.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed group remains present");
        assert_eq!(tab.custom_title.as_deref(), Some("Design bank"));

        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed group survives restart");
        assert_eq!(tab.custom_title.as_deref(), Some("Design bank"));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tab_reorder_moves_whole_tabs_only_within_their_workstation() {
        let directory =
            std::env::temp_dir().join(format!("hh-tab-reorder-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let first_tab = initial.workspaces[0].tabs[0].id;
        let second_pane = registry.create_workspace_tab(workspace_id).unwrap();
        let third_pane = registry.create_workspace_tab(workspace_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];
        let second_tab = workspace
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, second_pane))
            .unwrap()
            .id;
        let third_tab = workspace
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, third_pane))
            .unwrap()
            .id;

        registry.reorder_tab(third_tab, first_tab, false).unwrap();
        registry.reorder_tab(first_tab, second_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![third_tab, second_tab, first_tab]
        );
        drop(snapshot);
        drop(registry);

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            vec![third_tab, second_tab, first_tab]
        );
        drop(snapshot);

        let (other_workspace, _) = registry.create_workspace(Some("Other")).unwrap();
        let other_tab = registry
            .snapshot()
            .unwrap()
            .workspaces
            .iter()
            .find(|workspace| workspace.id == other_workspace)
            .unwrap()
            .tabs[0]
            .id;
        assert!(registry.reorder_tab(first_tab, other_tab, false).is_err());
        drop(registry);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn merging_a_live_tab_into_another_strip_preserves_its_process() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let target = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let moved = registry.create_group_terminal(first).unwrap();
        let moved_pid = registry.pane_process_id(moved).unwrap();

        registry.move_pane_to_tab(moved, target).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected both tiled panes to remain after the merge");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Stack { panes, active } = &**right else {
            panic!("dragged terminal must join the target tab strip");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [target, moved]
        );
        assert_eq!(*active, moved);
        assert_eq!(registry.pane_process_id(moved).unwrap(), moved_pid);
    }

    #[test]
    fn sidebar_drag_moves_browser_into_group_and_back_to_a_top_level_tab() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let group_pane = registry.create_workspace_group(workspace_id, None).unwrap();
        let group_tab = registry.snapshot().unwrap().workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, group_pane))
            .unwrap()
            .id;
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry.move_pane_to_group(browser, group_tab).unwrap();

        let grouped = registry.snapshot().unwrap();
        assert_eq!(grouped.workspaces[0].tabs.len(), 2);
        let group = grouped.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == group_tab)
            .unwrap();
        let PaneLayout::Stack { panes, active } = &group.layout else {
            panic!("browser must join the target sidebar group");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [group_pane, browser]
        );
        assert_eq!(*active, browser);

        registry
            .move_pane_to_new_tab(browser, group_tab, true, None)
            .unwrap();

        let extracted = registry.snapshot().unwrap();
        assert_eq!(extracted.workspaces[0].tabs.len(), 3);
        let group_index = extracted.workspaces[0]
            .tabs
            .iter()
            .position(|tab| tab.id == group_tab)
            .unwrap();
        assert!(matches!(
            &extracted.workspaces[0].tabs[group_index].layout,
            PaneLayout::Leaf { pane } if pane.id == group_pane
        ));
        assert!(matches!(
            &extracted.workspaces[0].tabs[group_index + 1].layout,
            PaneLayout::Leaf { pane }
                if pane.id == browser
                    && matches!(pane.kind, PaneKind::Browser { .. })
        ));
    }

    #[test]
    fn closing_one_tab_terminates_only_that_tab_and_preserves_its_pane_group() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let closing = registry.create_group_terminal(first).unwrap();

        registry.close_pane(closing).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert!(matches!(
            &snapshot.workspaces[0].tabs[0].layout,
            PaneLayout::Leaf { pane } if pane.id == first
        ));
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert!(registry.pane_process_id(closing).is_err());
    }

    #[test]
    fn pane_local_tab_actions_only_mutate_the_explicit_second_pane() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let second_tab = registry.create_group_terminal(second).unwrap();
        registry.rename_pane(second_tab, "Second pane tab").unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected two pane columns");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Stack { panes, active } = &**right else {
            panic!("new tab must be placed in the targeted second pane");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [second, second_tab]
        );
        assert_eq!(*active, second_tab);
        assert_eq!(panes[1].title, "Second pane tab");

        registry.activate_tab(second).unwrap();
        registry.close_pane(second_tab).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("closing a second-pane tab must preserve both pane columns");
        };
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        assert!(matches!(&**right, PaneLayout::Leaf { pane } if pane.id == second));
    }

    #[test]
    fn reorder_next_to_a_project_child_adopts_the_project() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let moved_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry.reorder_tab(moved_tab, child_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let moved_index = tabs.iter().position(|tab| tab.id == moved_tab).unwrap();
        let child_index = tabs.iter().position(|tab| tab.id == child_tab).unwrap();
        assert_eq!(tabs[moved_index].parent_tab, Some(project_tab));
        assert_eq!(moved_index, child_index + 1);
    }

    #[test]
    fn reorder_next_to_a_root_tab_unnests_a_project_child() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let root_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry.reorder_tab(child_tab, root_tab, true).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let root_index = tabs.iter().position(|tab| tab.id == root_tab).unwrap();
        let child_index = tabs.iter().position(|tab| tab.id == child_tab).unwrap();
        assert_eq!(tabs[child_index].parent_tab, None);
        assert_eq!(child_index, root_index + 1);
    }

    #[test]
    fn projects_never_nest() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let parent_project = {
            let pane = registry
                .create_workspace_project(workspace_id, "/tmp", Some("Project A"))
                .unwrap();
            tab_id_for_pane(&registry.snapshot().unwrap(), pane)
        };
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(parent_project))
            .unwrap();
        let child = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);
        let moving_project = {
            let pane = registry
                .create_workspace_project(workspace_id, "/tmp", Some("Project B"))
                .unwrap();
            tab_id_for_pane(&registry.snapshot().unwrap(), pane)
        };

        let error = registry
            .move_tab_to_project(moving_project, parent_project)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("a project cannot nest inside another project")
        );

        registry.reorder_tab(moving_project, child, true).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let moved_project = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == moving_project)
            .unwrap();
        assert_eq!(moved_project.parent_tab, None);
    }

    #[test]
    fn move_tab_to_project_appends_after_the_last_child() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let moved_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let first_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let first_child = tab_id_for_pane(&registry.snapshot().unwrap(), first_child_pane);
        let second_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let second_child = tab_id_for_pane(&registry.snapshot().unwrap(), second_child_pane);

        registry
            .move_tab_to_project(moved_tab, project_tab)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let tabs = &snapshot.workspaces[0].tabs;
        let project_index = tabs.iter().position(|tab| tab.id == project_tab).unwrap();
        assert_eq!(tabs[project_index + 1].id, first_child);
        assert_eq!(tabs[project_index + 2].id, second_child);
        assert_eq!(tabs[project_index + 3].id, moved_tab);
        assert_eq!(tabs[project_index + 3].parent_tab, Some(project_tab));

        let revision = snapshot.revision;
        registry
            .move_tab_to_project(moved_tab, project_tab)
            .unwrap();
        assert_eq!(registry.snapshot().unwrap().revision, revision);
    }

    #[test]
    fn pane_detached_into_a_project_becomes_a_child_tab() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let initial_pane = first_pane_id(&initial).unwrap();
        let detached_pane = registry.create_group_terminal(initial_pane).unwrap();
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let target_child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let target_child = tab_id_for_pane(&registry.snapshot().unwrap(), target_child_pane);

        registry
            .move_pane_to_new_tab(detached_pane, project_tab, false, Some(project_tab))
            .unwrap();
        let snapshot = registry.snapshot().unwrap();
        let detached_tab = tab_id_for_pane(&snapshot, detached_pane);
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .find(|tab| tab.id == detached_tab)
                .unwrap()
                .parent_tab,
            Some(project_tab)
        );

        let source_group_pane = registry.create_workspace_group(workspace_id, None).unwrap();
        let adopted_pane = registry.create_group_terminal(source_group_pane).unwrap();
        registry
            .move_pane_to_new_tab(adopted_pane, target_child, true, None)
            .unwrap();
        let snapshot = registry.snapshot().unwrap();
        let adopted_tab = tab_id_for_pane(&snapshot, adopted_pane);
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .find(|tab| tab.id == adopted_tab)
                .unwrap()
                .parent_tab,
            Some(project_tab)
        );
    }

    #[test]
    fn moving_a_project_pane_into_a_group_unnests_its_children() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let target_tab = initial.workspaces[0].tabs[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry
            .move_pane_to_group(project_pane, target_tab)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let child = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == child_tab)
            .unwrap();
        assert_eq!(child.parent_tab, None);
        assert!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .all(|tab| tab.id != project_tab)
        );
    }

    #[test]
    fn moving_a_project_pane_next_to_its_child_unnests_both_tabs() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let project_pane = registry
            .create_workspace_project(workspace_id, "/tmp", Some("Project"))
            .unwrap();
        let project_tab = tab_id_for_pane(&registry.snapshot().unwrap(), project_pane);
        let child_pane = registry
            .create_workspace_group(workspace_id, Some(project_tab))
            .unwrap();
        let child_tab = tab_id_for_pane(&registry.snapshot().unwrap(), child_pane);

        registry
            .move_pane_to_new_tab(project_pane, child_tab, false, None)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let child = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == child_tab)
            .unwrap();
        assert_eq!(child.parent_tab, None);
        let moved_tab = tab_id_for_pane(&snapshot, project_pane);
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .find(|tab| tab.id == moved_tab)
                .unwrap()
                .parent_tab,
            None
        );
        assert!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .all(|tab| tab.id != project_tab)
        );
    }

    #[test]
    fn move_pane_to_new_tab_keeps_a_full_workspace_at_its_limit() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let source_pane = first_pane_id(&initial).unwrap();
        let mut target_pane = None;
        for _ in 1..MAX_TABS_PER_WORKSPACE {
            let pane = registry.create_browser_tab(workspace_id, None).unwrap();
            target_pane.get_or_insert(pane);
        }
        let before = registry.snapshot().unwrap();
        let target_tab = tab_id_for_pane(&before, target_pane.unwrap());

        registry
            .move_pane_to_new_tab(source_pane, target_tab, false, None)
            .unwrap();

        let after = registry.snapshot().unwrap();
        assert_eq!(after.workspaces[0].tabs.len(), MAX_TABS_PER_WORKSPACE);
        assert_ne!(tab_id_for_pane(&after, source_pane), target_tab);
    }

    #[test]
    fn set_tab_pinned_round_trips() {
        let registry = SessionRegistry::new().unwrap();
        let tab_id = registry.snapshot().unwrap().workspaces[0].tabs[0].id;

        registry.set_tab_pinned(tab_id, true).unwrap();
        assert!(registry.snapshot().unwrap().workspaces[0].tabs[0].pinned);

        registry.set_tab_pinned(tab_id, false).unwrap();
        assert!(!registry.snapshot().unwrap().workspaces[0].tabs[0].pinned);

        let error = registry.set_tab_pinned(Uuid::new_v4(), true).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }
}
