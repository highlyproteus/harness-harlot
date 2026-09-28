//! Tab operations: layout moves, tab metadata, and reorder within workstations.
use super::{
    ProcessScan, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    TerminalRuntimePane, encode_desired_state, tmux_session_name,
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
use crate::registry::bots::prune_bot_threads;
use crate::registry::identity::refresh_workspace_activity;
use crate::registry::workspaces::{remember_recent_color, same_machine};
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, DropPlacement, MAX_PANES, PaneLayout, Tab, effective_working_dir,
};
use std::sync::Arc;
use uuid::Uuid;

impl SessionRegistry {
    /// Appends one more top-level tab to a workstation, opening its terminal
    /// in the workstation's effective root folder. Unlike
    /// `create_workspace_terminal` this is deliberately not idempotent: every
    /// request adds a tab, which is what the workstation menu's "New Tab"
    /// means.
    pub fn create_workspace_tab(&self, workspace_id: Uuid) -> Result<Uuid> {
        self.ensure_workspace_accepts_workstation_tabs(workspace_id)?;
        let root = {
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
            effective_working_dir(&state.snapshot.workspaces, workspace_id).map(str::to_owned)
        };

        let pane_id = Uuid::new_v4();
        let cwd = local_spawn_dir(root.as_deref())?;
        let (session, kind) =
            self.spawn_pane_for_workspace(pane_id, workspace_id, &cwd, root.as_deref())?;
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
            let pane = state.new_runtime_pane(pane_id, &cwd, &kind);
            let workspace = &mut state.snapshot.workspaces[workspace_index];
            workspace.tabs.push(Tab {
                owner_thread: None,
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title: None,
                color: None,
                custom_icon: None,
                pinned: false,
                owner_bot: None,
                layout: PaneLayout::Leaf { pane },
            });
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
                        process_scan: ProcessScan::Unknown,
                        omp_title_status: None,
                        title_baseline_pending: false,
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
        let now = crate::now_ms();
        let did_activate = state.snapshot.workspaces.iter_mut().any(|workspace| {
            let activated = workspace
                .tabs
                .iter_mut()
                .any(|tab| activate_tab(&mut tab.layout, pane_id));
            if activated && let Some(spec) = &mut workspace.bot {
                spec.thread_panes.entry(pane_id).or_default().activated_ms = now;
            }
            activated
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
        self.state.read().refuse_bot_pane(pane_id)?;
        let replacement_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(pane_id)?;
        let workspace_id = self.workspace_for_pane(pane_id)?;
        let replacement_session =
            self.spawn_local_transport(replacement_id, workspace_id, None, &cwd)?;
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
                        process_scan: ProcessScan::Unknown,
                        omp_title_status: None,
                        title_baseline_pending: false,
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

    pub fn move_pane_into_tab(&self, source_pane: Uuid, target_tab: Uuid) -> Result<()> {
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
        if source_location == target_location {
            return Ok(());
        }

        let workspace = &mut state.snapshot.workspaces[source_location.0];
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        let mut target_layout = workspace.tabs[target_location.1].layout.clone();
        let target_pane = first_layout_pane(&target_layout);
        if !add_tab(&mut target_layout, target_pane, pane, true) {
            bail!("target tab {target_tab} cannot accept pane {source_pane}");
        }
        workspace.tabs[target_location.1].layout = target_layout;
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            workspace.tabs.remove(source_location.1);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(())
    }

    pub fn move_pane_to_new_tab(
        &self,
        source_pane: Uuid,
        target_tab: Uuid,
        after: bool,
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
        let source_layout = workspace.tabs[source_location.1].layout.clone();
        let (pane, remaining) = detach_pane(source_layout, source_pane);
        let pane = pane.with_context(|| format!("source pane {source_pane} does not exist"))?;
        if remaining.is_none() && workspace.tabs[source_location.1].id == target_tab {
            return Ok(());
        }
        if remaining.is_some() && workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        if let Some(remaining) = remaining {
            workspace.tabs[source_location.1].layout = remaining;
        } else {
            workspace.tabs.remove(source_location.1);
        }
        let target_index = workspace
            .tabs
            .iter()
            .position(|tab| tab.id == target_tab)
            .with_context(|| format!("target tab {target_tab} disappeared during the move"))?;
        let insertion_index = target_index + usize::from(after);
        workspace.tabs.insert(
            insertion_index,
            Tab {
                owner_thread: None,
                id: Uuid::new_v4(),
                title: pane.title.clone(),
                custom_title: None,
                color: None,
                custom_icon: None,
                pinned: false,
                owner_bot: None,
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
        validate_title(title, "tab")?;
        let mut state = self.state.write();
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| workspace.tabs.iter_mut())
            .find(|tab| tab.id == tab_id)
            .with_context(|| format!("tab {tab_id} does not exist"))?;
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
        let previous_snapshot = state.snapshot.clone();
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
        if let Err(error) = self.write_snapshot(&bytes) {
            state.snapshot = previous_snapshot;
            return Err(error);
        }
        Ok(())
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
            let mut pane_ids = Vec::new();
            for tab in state.snapshot.workspaces[workspace_index]
                .tabs
                .iter()
                .filter(|tab| tab.id == tab_id)
            {
                collect_pane_ids(&tab.layout, &mut pane_ids);
            }
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
            workspace.tabs.retain(|tab| tab.id != tab_id);
            workspace.active_terminal_count = workspace
                .active_terminal_count
                .saturating_sub(terminal_count);
            prune_bot_threads(workspace);
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
        let tab = tabs.remove(source);
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

    /// Moves a tab, with its panes, to the end of another workstation on the
    /// same machine. Live managed tmux windows move into the target
    /// workstation's tmux session and are reattached there, so their
    /// processes keep running and survive the next restart.
    pub fn move_tab_to_workstation(&self, tab_id: Uuid, workspace_id: Uuid) -> Result<()> {
        let (source_id, relocations) = {
            let state = self.state.read();
            let source = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.tabs.iter().any(|tab| tab.id == tab_id))
                .with_context(|| format!("tab {tab_id} does not exist"))?;
            let target = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if source.id == target.id {
                return Ok(());
            }
            if source.is_bot() || target.is_bot() {
                bail!("tabs of a bot cannot move between workstations");
            }
            if !same_machine(&source.connection, &target.connection) {
                bail!("tabs can only move between workstations on the same machine");
            }
            if target.tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
            }
            let mut pane_ids = Vec::new();
            for tab in source.tabs.iter().filter(|tab| tab.id == tab_id) {
                collect_pane_ids(&tab.layout, &mut pane_ids);
            }
            let relocations = pane_ids
                .into_iter()
                .filter_map(|pane_id| {
                    let terminal = state.panes.get(&pane_id)?.terminal()?;
                    if terminal.kind != RuntimePaneKind::Local
                        || terminal.session.exit_status().ok()?.is_some()
                    {
                        return None;
                    }
                    let (window_id, tmux_pane_id) = terminal.session.tmux_ids()?;
                    Some((
                        pane_id,
                        window_id.to_owned(),
                        tmux_pane_id.to_owned(),
                        terminal.session.process_id()?,
                    ))
                })
                .collect::<Vec<_>>();
            (source.id, relocations)
        };

        let mut reattached = Vec::with_capacity(relocations.len());
        if !relocations.is_empty() {
            let client = self.client_for_workspace(workspace_id)?;
            let relocated = (|| {
                for (pane_id, window_id, tmux_pane_id, pane_pid) in &relocations {
                    client.move_window_to_session(window_id, &tmux_session_name(workspace_id))?;
                    reattached.push((
                        *pane_id,
                        PtySession::attach_tmux(
                            *pane_id,
                            Arc::clone(&client),
                            window_id.clone(),
                            tmux_pane_id.clone(),
                            *pane_pid,
                        )?,
                    ));
                }
                Ok::<_, anyhow::Error>(())
            })();
            if let Err(error) = relocated {
                reattached.clear();
                for (_, window_id, _, _) in &relocations {
                    let _ = client.move_window_to_session(window_id, &tmux_session_name(source_id));
                }
                return Err(error).context("move the tab's tmux windows");
            }
        }

        let mut state = self.state.write();
        let source_index = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.id == source_id)
            .context("source workstation disappeared while moving a tab")?;
        let tab_index = state.snapshot.workspaces[source_index]
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .context("tab disappeared while moving it")?;
        let target_index = state
            .snapshot
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
            .context("target workstation disappeared while moving a tab")?;
        let tab = state.snapshot.workspaces[source_index]
            .tabs
            .remove(tab_index);
        let mut pane_ids = Vec::new();
        collect_pane_ids(&tab.layout, &mut pane_ids);
        state.snapshot.workspaces[target_index].tabs.push(tab);
        let mut replaced = Vec::with_capacity(reattached.len());
        for (pane_id, session) in reattached {
            if let Ok(terminal) = state.terminal_pane_mut(pane_id) {
                replaced.push(std::mem::replace(&mut terminal.session, session));
            }
        }
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(replaced);
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
    fn tab_names_are_validated_and_survive_restart() {
        let directory = std::env::temp_dir().join(format!("hh-tab-name-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;

        let pane_id = registry.create_workspace_tab(workspace_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, pane_id))
            .expect("new tab owns the returned pane");
        let tab_id = tab.id;
        assert_eq!(tab.custom_title, None);

        registry.rename_tab(tab_id, "  Design bank  ").unwrap();
        assert!(registry.rename_tab(tab_id, "").is_err());
        assert!(registry.rename_tab(tab_id, &"x".repeat(81)).is_err());
        let renamed = registry.snapshot().unwrap();
        let tab = renamed.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed tab remains present");
        assert_eq!(tab.custom_title.as_deref(), Some("Design bank"));

        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let tab = snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .expect("renamed tab survives restart");
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

        let (other_workspace, _) = registry
            .create_workspace(Some("Other"), None, None)
            .unwrap();
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
        let moved = registry.create_tab_terminal(first).unwrap();
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
    fn sidebar_drag_moves_browser_into_a_tab_and_back_to_its_own_tab() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let group_pane = registry.create_workspace_tab(workspace_id).unwrap();
        let group_tab = registry.snapshot().unwrap().workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, group_pane))
            .unwrap()
            .id;
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry.move_pane_into_tab(browser, group_tab).unwrap();

        let grouped = registry.snapshot().unwrap();
        assert_eq!(grouped.workspaces[0].tabs.len(), 2);
        let group = grouped.workspaces[0]
            .tabs
            .iter()
            .find(|tab| tab.id == group_tab)
            .unwrap();
        let PaneLayout::Stack { panes, active } = &group.layout else {
            panic!("browser must join the target tab");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [group_pane, browser]
        );
        assert_eq!(*active, browser);

        registry
            .move_pane_to_new_tab(browser, group_tab, true)
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
        let closing = registry.create_tab_terminal(first).unwrap();

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
        let second_tab = registry.create_tab_terminal(second).unwrap();
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
            .move_pane_to_new_tab(source_pane, target_tab, false)
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

    #[test]
    fn a_tab_moves_only_to_another_workstation_on_the_same_machine() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let home = initial.workspaces[0].id;
        let tab_id = initial.workspaces[0].tabs[0].id;
        let pane_id = first_pane_id(&initial).unwrap();
        let pid = registry.pane_process_id(pane_id).unwrap();
        let (remote, _) = registry
            .create_simulated_ssh_workspace(Some("Remote"), "test@local-host")
            .unwrap();
        let (nested, _) = registry.create_workspace(None, Some(home), None).unwrap();

        let error = registry
            .move_tab_to_workstation(tab_id, remote)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "tabs can only move between workstations on the same machine"
        );

        registry.move_tab_to_workstation(tab_id, nested).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let find = |id| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == id)
                .unwrap()
        };
        assert!(find(home).tabs.is_empty());
        assert_eq!(find(nested).tabs.last().map(|tab| tab.id), Some(tab_id));
        assert_eq!(find(nested).active_terminal_count, 2);
        assert_eq!(find(home).active_terminal_count, 0);
        assert_eq!(registry.pane_process_id(pane_id).unwrap(), pid);
    }
}
