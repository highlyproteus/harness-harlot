//! Workstation lifecycle: creation, SSH intents, pins, order, and appearance defaults.
use super::{
    RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry, SshWorkspaceIds,
    TerminalRuntimePane, encode_desired_state,
};
use crate::history::HistoryArchive;
use crate::layout::{find_pane_mut, pane_ids_for_workspace};
use crate::persistence::{
    MAX_INSTRUCTIONS_CHARS, MAX_RECENT_COLORS, MAX_WORKSPACES, validate_title,
};
use crate::process::fallback_cwd;
use crate::pty::PtySession;
use crate::registry::identity::set_pane_runtime_label;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, MAX_PANES, Pane, PaneLayout, SessionSnapshot, Tab, TerminalIdentity,
    Workspace, WorkspaceConnection, WorkspaceConnectionStatus, WorkspaceKind, WorkspacePinMove,
    validate_ssh_host, validate_workspace_dir,
};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn remember_recent_color(snapshot: &mut SessionSnapshot, color: AppearanceColor) {
    snapshot
        .appearance
        .recent_colors
        .retain(|recent| *recent != color);
    snapshot.appearance.recent_colors.insert(0, color);
    snapshot
        .appearance
        .recent_colors
        .truncate(MAX_RECENT_COLORS);
}

/// One offline SSH workstation selected for reconnection.
#[derive(Debug)]
struct ReconnectionPlan {
    destination: String,
    working_dir: Option<String>,
    pane_ids: Vec<Uuid>,
}

/// Spawns every SSH session a reconnect needs, terminating any already
/// spawned session when one fails.
fn spawn_reconnect_sessions(
    workspace_id: Uuid,
    destination: &str,
    working_dir: Option<&str>,
    pane_ids: &[Uuid],
    history: &HistoryArchive,
) -> Result<Vec<(Uuid, Arc<PtySession>)>> {
    let mut sessions = Vec::with_capacity(pane_ids.len());
    for pane_id in pane_ids {
        match PtySession::spawn_ssh(*pane_id, workspace_id, destination, working_dir, history) {
            Ok(session) => sessions.push((*pane_id, session)),
            Err(error) => {
                for (_, session) in sessions {
                    let _ = session.terminate_and_wait();
                }
                return Err(error);
            }
        }
    }
    Ok(sessions)
}

pub(crate) fn normalize_workspace_title(title: Option<&str>) -> Result<Option<String>> {
    let Some(title) = title else {
        return Ok(None);
    };
    let title = title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    validate_title(title, "workstation")?;
    Ok(Some(title.to_owned()))
}

fn normalize_assistant_instructions(instructions: Option<String>) -> Result<Option<String>> {
    let instructions = instructions
        .map(|instructions| instructions.trim().to_owned())
        .filter(|instructions| !instructions.is_empty());
    if instructions
        .as_deref()
        .is_some_and(|instructions| instructions.chars().count() > MAX_INSTRUCTIONS_CHARS)
    {
        bail!("assistant instructions too long");
    }
    Ok(instructions)
}

pub(crate) fn next_workspace_order(workspaces: &[Workspace], pinned: bool) -> u32 {
    workspaces
        .iter()
        .filter(|workspace| workspace.pinned == pinned)
        .map(|workspace| workspace.order)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

pub(crate) fn normalize_workspace_orders(workspaces: &mut [Workspace]) {
    let mut pinned = workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| workspace.pinned)
        .map(|(index, workspace)| (index, workspace.pin_order))
        .collect::<Vec<_>>();
    pinned.sort_by_key(|(index, order)| (*order, *index));
    for (order, (index, _)) in pinned.into_iter().enumerate() {
        workspaces[index].pin_order = u32::try_from(order + 1).unwrap_or(u32::MAX);
    }
    for workspace in workspaces.iter_mut().filter(|workspace| !workspace.pinned) {
        workspace.pin_order = 0;
    }
    for pinned in [true, false] {
        let mut group = workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| workspace.pinned == pinned)
            .map(|(index, workspace)| (index, workspace.order))
            .collect::<Vec<_>>();
        group.sort_by_key(|(index, order)| (*order, *index));
        for (order, (index, _)) in group.into_iter().enumerate() {
            workspaces[index].order = u32::try_from(order + 1).unwrap_or(u32::MAX);
        }
    }
}

impl SessionRegistry {
    pub(crate) fn ensure_workspace_accepts_non_assistant_tabs(
        &self,
        workspace_id: Uuid,
    ) -> Result<()> {
        let state = self.state.read();
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.is_assistant() {
            bail!("assistant workspaces only hold assistant threads");
        }
        Ok(())
    }

    pub fn set_default_terminal_accent(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self.state.write();
        state.snapshot.appearance.default_terminal_accent = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_default_workspace_color(&self, color: AppearanceColor) -> Result<()> {
        let mut state = self.state.write();
        state.snapshot.appearance.default_workspace_color = color;
        remember_recent_color(&mut state.snapshot, color);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_workspace_color(
        &self,
        workspace_id: Uuid,
        color: Option<AppearanceColor>,
    ) -> Result<()> {
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.color = color;
        if let Some(color) = color {
            remember_recent_color(&mut state.snapshot, color);
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_workspace_working_dir(
        &self,
        workspace_id: Uuid,
        working_dir: Option<String>,
    ) -> Result<()> {
        if let Some(dir) = working_dir.as_deref() {
            validate_workspace_dir(dir).map_err(anyhow::Error::from)?;
        }
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.working_dir == working_dir {
            return Ok(());
        }
        workspace.working_dir = working_dir;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_workspace_custom_icon(
        &self,
        workspace_id: Uuid,
        icon: Option<String>,
    ) -> Result<()> {
        if let Some(icon) = icon.as_deref() {
            crate::persistence::validate_custom_icon_id(icon)?;
        }
        let mut state = self.state.write();
        let previous_snapshot = state.snapshot.clone();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.custom_icon == icon {
            return Ok(());
        }
        workspace.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        if let Err(error) = self.write_snapshot(&bytes) {
            state.snapshot = previous_snapshot;
            return Err(error);
        }
        Ok(())
    }

    pub fn create_workspace(&self, title: Option<&str>) -> Result<(Uuid, Uuid)> {
        let title = normalize_workspace_title(title)?;
        {
            let state = self.state.read();
            if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
        }
        let workspace_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let session = PtySession::spawn_local(pane_id, workspace_id, &cwd, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let number = state.snapshot.workspaces.len() + 1;
            let order = next_workspace_order(&state.snapshot.workspaces, false);
            let pane = state.new_pane(pane_id, Some(cwd.as_path()));
            state.snapshot.workspaces.push(Workspace {
                id: workspace_id,
                title: title.unwrap_or_else(|| format!("Workstation {number}")),
                color: None,
                pinned: false,
                pin_order: 0,
                order,
                active_terminal_count: 1,
                connection: WorkspaceConnection::Local,
                working_dir: None,
                kind: WorkspaceKind::Workstation,
                instructions: None,
                custom_icon: None,
                tabs: vec![Tab {
                    id: tab_id,
                    title: "Terminals".to_owned(),
                    custom_title: None,
                    project_dir: None,
                    color: None,
                    custom_icon: None,
                    parent_tab: None,
                    pinned: false,
                    layout: PaneLayout::Leaf { pane },
                }],
            });
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::Local,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            );
            state.snapshot.revision += 1;
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok((workspace_id, pane_id))
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }
    pub fn create_assistant_workspace(
        &self,
        title: Option<&str>,
        working_dir: Option<String>,
        instructions: Option<String>,
    ) -> Result<(Uuid, Uuid)> {
        let title = normalize_workspace_title(title)?;
        if let Some(working_dir) = working_dir.as_deref() {
            validate_workspace_dir(working_dir).map_err(anyhow::Error::from)?;
        }
        let instructions = normalize_assistant_instructions(instructions)?;
        {
            let state = self.state.read();
            if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
        }

        let workspace_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
            bail!("workstation limit of {MAX_WORKSPACES} reached");
        }
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let number = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.is_assistant())
            .count()
            + 1;
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        state.snapshot.workspaces.push(Workspace {
            id: workspace_id,
            title: title.unwrap_or_else(|| format!("Assistant {number}")),
            color: None,
            pinned: false,
            pin_order: 0,
            order,
            active_terminal_count: 0,
            connection: WorkspaceConnection::Local,
            working_dir,
            kind: WorkspaceKind::Assistant,
            instructions,
            custom_icon: None,
            tabs: vec![Tab {
                id: tab_id,
                title: "Thread 1".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf {
                    pane: Pane {
                        id: pane_id,
                        title: "Assistant".to_owned(),
                        shell: String::new(),
                        kind: hh_protocol::PaneKind::Assistant,
                        color: None,
                        identity: TerminalIdentity::default(),
                        status: hh_protocol::PaneStatus::default(),
                        custom_title: None,
                        profile_override: None,
                        custom_icon: None,
                    },
                },
            }],
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Assistant,
            },
        );
        let previous_revision = state.snapshot.revision;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        if let Err(error) = self.write_snapshot(&bytes) {
            state
                .snapshot
                .workspaces
                .retain(|workspace| workspace.id != workspace_id);
            state.panes.remove(&pane_id);
            state.snapshot.revision = previous_revision;
            return Err(error);
        }
        Ok((workspace_id, pane_id))
    }

    pub fn create_ssh_workspace(
        &self,
        title: Option<&str>,
        destination: &str,
    ) -> Result<(Uuid, Uuid)> {
        validate_ssh_host(destination).map_err(anyhow::Error::from)?;
        let title = normalize_workspace_title(title)?;
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };
        let cwd = fallback_cwd()?;
        self.persist_ssh_workspace_intent(title, destination, ids)?;
        let session =
            PtySession::spawn_ssh(ids.pane, ids.workspace, destination, None, &self.history)?;
        let result = self.attach_ssh_workspace(destination, ids, cwd, Arc::clone(&session));
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub(crate) fn persist_ssh_workspace_intent(
        &self,
        title: Option<String>,
        destination: &str,
        ids: SshWorkspaceIds,
    ) -> Result<()> {
        let mut state = self.state.write();
        if state.snapshot.workspaces.len() >= MAX_WORKSPACES {
            bail!("workstation limit of {MAX_WORKSPACES} reached");
        }
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let number = state.snapshot.workspaces.len() + 1;
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        let pane = Pane {
            id: ids.pane,
            kind: hh_protocol::PaneKind::Terminal,
            title: format!("SSH {destination}"),
            shell: "ssh".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        state.snapshot.workspaces.push(Workspace {
            id: ids.workspace,
            title: title.unwrap_or_else(|| format!("SSH Workstation {number}")),
            color: None,
            pinned: false,
            pin_order: 0,
            order,
            active_terminal_count: 0,
            connection: WorkspaceConnection::SystemSsh {
                destination: destination.to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            },
            working_dir: None,
            kind: WorkspaceKind::Workstation,
            instructions: None,
            custom_icon: None,
            tabs: vec![Tab {
                id: ids.tab,
                title: "Remote".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            }],
        });
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub(crate) fn attach_ssh_workspace(
        &self,
        destination: &str,
        ids: SshWorkspaceIds,
        cwd: PathBuf,
        session: Arc<PtySession>,
    ) -> Result<(Uuid, Uuid)> {
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == ids.workspace)
            .context("saved SSH workstation disappeared before session attachment")?;
        let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
            bail!("saved workstation connection type changed before session attachment");
        };
        *status = WorkspaceConnectionStatus::Connected;
        workspace.active_terminal_count = 1;
        state.panes.insert(
            ids.pane,
            RuntimePane {
                backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                    session,
                    last_valid_cwd: cwd,
                    kind: RuntimePaneKind::SystemSsh {
                        host: destination.to_owned(),
                    },
                    recovered: false,
                    exit_status: None,
                    detected_command_profile: None,
                    omp_title_status: None,
                }),
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok((ids.workspace, ids.pane))
    }

    #[cfg(test)]
    pub(crate) fn create_simulated_ssh_workspace(
        &self,
        title: Option<&str>,
        destination: &str,
    ) -> Result<(Uuid, Uuid)> {
        validate_ssh_host(destination).map_err(anyhow::Error::from)?;
        let title = normalize_workspace_title(title)?;
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };
        let cwd = fallback_cwd()?;
        self.persist_ssh_workspace_intent(title, destination, ids)?;
        let session = PtySession::spawn_local(ids.pane, ids.workspace, &cwd, &self.history)?;
        let result = self.attach_ssh_workspace(destination, ids, cwd, Arc::clone(&session));
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn rename_workspace(&self, workspace_id: Uuid, title: &str) -> Result<()> {
        let title =
            normalize_workspace_title(Some(title))?.context("workstation name cannot be empty")?;
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.title = title;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn set_workspace_pinned(&self, workspace_id: Uuid, pinned: bool) -> Result<()> {
        let mut state = self.state.write();
        let next_order = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned)
            .map(|workspace| workspace.pin_order)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let unpinned_order = next_workspace_order(&state.snapshot.workspaces, false);
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        workspace.pinned = pinned;
        workspace.pin_order = if pinned { next_order } else { 0 };
        if !pinned {
            workspace.order = unpinned_order;
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn move_pinned_workspace(
        &self,
        workspace_id: Uuid,
        direction: WorkspacePinMove,
    ) -> Result<()> {
        let mut state = self.state.write();
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        let mut pinned = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned)
            .map(|workspace| (workspace.id, workspace.pin_order))
            .collect::<Vec<_>>();
        pinned.sort_by_key(|(_, order)| *order);
        let index = pinned
            .iter()
            .position(|(id, _)| *id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} is not pinned"))?;
        let other = match direction {
            WorkspacePinMove::Up => index.checked_sub(1),
            WorkspacePinMove::Down => (index + 1 < pinned.len()).then_some(index + 1),
        };
        let Some(other) = other else {
            return Ok(());
        };
        let first = pinned[index];
        let second = pinned[other];
        for workspace in &mut state.snapshot.workspaces {
            if workspace.id == first.0 {
                workspace.pin_order = second.1;
            } else if workspace.id == second.0 {
                workspace.pin_order = first.1;
            }
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn reorder_workspace(
        &self,
        workspace_id: Uuid,
        target_workspace_id: Uuid,
        after: bool,
    ) -> Result<()> {
        let mut state = self.state.write();
        let source_pinned = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?
            .pinned;
        let target_pinned = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target_workspace_id)
            .with_context(|| format!("workstation {target_workspace_id} does not exist"))?
            .pinned;
        if source_pinned != target_pinned {
            bail!("workstations can only be reordered within the same group");
        }
        let mut ordered = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned == source_pinned)
            .map(|workspace| (workspace.id, workspace.order))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(_, order)| *order);
        let source = ordered
            .iter()
            .position(|(id, _)| *id == workspace_id)
            .context("source workstation was not in its pinned group")?;
        let item = ordered.remove(source);
        let target = ordered
            .iter()
            .position(|(id, _)| *id == target_workspace_id)
            .context("target workstation was not in its pinned group")?;
        ordered.insert(target + usize::from(after), item);
        for (index, (id, _)) in ordered.into_iter().enumerate() {
            if let Some(workspace) = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
            {
                workspace.order = u32::try_from(index + 1).unwrap_or(u32::MAX);
            }
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn disconnect_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let sessions = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. }) {
                bail!("only a system-SSH workstation can be disconnected");
            }
            pane_ids_for_workspace(workspace)
                .into_iter()
                .filter_map(|pane_id| {
                    let terminal = state.panes.get(&pane_id)?.terminal()?;
                    matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                        .then(|| (pane_id, Arc::clone(&terminal.session)))
                })
                .collect::<Vec<_>>()
        };
        for (_, session) in &sessions {
            let _ = session.terminate_and_wait();
        }

        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| {
                format!("workstation {workspace_id} disappeared while disconnecting")
            })?;
        let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
            bail!("only a system-SSH workstation can be disconnected");
        };
        *status = WorkspaceConnectionStatus::Offline;
        workspace.active_terminal_count = workspace
            .active_terminal_count
            .saturating_sub(u32::try_from(sessions.len()).unwrap_or(u32::MAX));
        for (pane_id, _) in sessions {
            if let Ok(runtime) = state.terminal_pane_mut(pane_id) {
                runtime.exit_status = Some("disconnected".to_owned());
            }
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                false,
                Some("disconnected"),
                "system OpenSSH",
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    pub fn reconnect_workspace(&self, workspace_id: Uuid) -> Result<Uuid> {
        let plan = self.reconnection_plan(workspace_id)?;
        validate_ssh_host(&plan.destination).map_err(anyhow::Error::from)?;
        let created_layout = plan.pane_ids.is_empty();
        let mut pane_ids = plan.pane_ids;
        if created_layout {
            pane_ids.push(Uuid::new_v4());
        }
        let sessions = spawn_reconnect_sessions(
            workspace_id,
            &plan.destination,
            plan.working_dir.as_deref(),
            &pane_ids,
            &self.history,
        )?;
        let result = self.apply_workspace_reconnection(
            workspace_id,
            &plan.destination,
            created_layout,
            &pane_ids,
            &sessions,
        );
        if result.is_err() {
            for (_, session) in sessions {
                let _ = session.terminate_and_wait();
            }
        }
        result
    }

    /// Reads the offline SSH destination and the pane IDs a reconnect must
    /// respawn under the write lock taken later.
    fn reconnection_plan(&self, workspace_id: Uuid) -> Result<ReconnectionPlan> {
        let state = self.state.read();
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        let WorkspaceConnection::SystemSsh {
            destination,
            status,
        } = &workspace.connection
        else {
            bail!("only a system-SSH workstation can be reconnected");
        };
        if *status == WorkspaceConnectionStatus::Connected {
            bail!("workstation is already connected");
        }
        let pane_ids = pane_ids_for_workspace(workspace)
            .into_iter()
            .filter(|pane_id| {
                state.panes.get(pane_id).is_none_or(|runtime| {
                    runtime.terminal().is_some_and(|terminal| {
                        matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                            && terminal.exit_status.is_some()
                    })
                })
            })
            .collect::<Vec<_>>();
        Ok(ReconnectionPlan {
            destination: destination.clone(),
            working_dir: workspace.working_dir.clone(),
            pane_ids,
        })
    }

    /// Publishes respawned SSH sessions into the desired state and marks the
    /// workstation connected again.
    fn apply_workspace_reconnection(
        &self,
        workspace_id: Uuid,
        destination: &str,
        created_layout: bool,
        pane_ids: &[Uuid],
        sessions: &[(Uuid, Arc<PtySession>)],
    ) -> Result<Uuid> {
        let cwd = fallback_cwd()?;
        let mut state = self.state.write();
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| {
                format!("workstation {workspace_id} disappeared while reconnecting")
            })?;
        let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection else {
            bail!("only a system-SSH workstation can be reconnected");
        };
        if created_layout {
            let pane_id = pane_ids[0];
            let pane = Pane {
                id: pane_id,
                kind: hh_protocol::PaneKind::Terminal,
                title: format!("SSH {destination}"),
                shell: "ssh".to_owned(),
                color: None,
                identity: TerminalIdentity::default(),
                status: hh_protocol::PaneStatus::default(),
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: "Remote".to_owned(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            });
        } else {
            for pane_id in pane_ids {
                if let Some(pane) = workspace
                    .tabs
                    .iter_mut()
                    .find_map(|tab| find_pane_mut(&mut tab.layout, *pane_id))
                {
                    pane.title = format!("SSH {destination}");
                    "ssh".clone_into(&mut pane.shell);
                }
            }
        }
        *status = WorkspaceConnectionStatus::Connected;
        workspace.active_terminal_count = workspace
            .active_terminal_count
            .saturating_add(u32::try_from(sessions.len()).unwrap_or(u32::MAX));

        for (pane_id, session) in sessions {
            state.panes.insert(
                *pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(session),
                        last_valid_cwd: cwd.clone(),
                        kind: RuntimePaneKind::SystemSsh {
                            host: destination.to_owned(),
                        },
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_ids[0])
    }

    pub fn delete_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let (pane_ids, sessions) = {
            let state = self.state.read();
            if state.snapshot.workspaces.len() <= 1 {
                bail!("the last workstation cannot be deleted");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let pane_ids = pane_ids_for_workspace(workspace);
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
            (pane_ids, sessions)
        };
        for session in &sessions {
            let _ = session.terminate_and_wait();
        }
        let mut state = self.state.write();
        if state.snapshot.workspaces.len() <= 1 {
            bail!("the last workstation cannot be deleted");
        }
        let before = state.snapshot.workspaces.len();
        state
            .snapshot
            .workspaces
            .retain(|workspace| workspace.id != workspace_id);
        if state.snapshot.workspaces.len() == before {
            bail!("workstation {workspace_id} disappeared while deleting");
        }
        let removed = pane_ids
            .into_iter()
            .filter_map(|pane_id| state.panes.remove(&pane_id))
            .collect::<Vec<_>>();
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(removed);
        self.write_snapshot(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{find_pane_mut_in_snapshot, first_pane_id};
    use crate::registry::{
        SessionRegistry, create_owner_only_directory, runtime_kind_for_workspace,
    };
    use uuid::Uuid;

    #[test]
    fn extra_panes_in_saved_ssh_workstations_retain_the_saved_destination() {
        let destination = "admin@build-node";
        let ssh = WorkspaceConnection::SystemSsh {
            destination: destination.to_owned(),
            status: WorkspaceConnectionStatus::Connected,
        };
        assert_eq!(
            runtime_kind_for_workspace(&ssh),
            RuntimePaneKind::SystemSsh {
                host: destination.to_owned(),
            }
        );
        assert_eq!(
            runtime_kind_for_workspace(&WorkspaceConnection::Local),
            RuntimePaneKind::Local
        );
    }

    #[test]
    fn failed_assistant_persistence_does_not_publish_live_state() {
        let directory =
            std::env::temp_dir().join(format!("hh-assistant-persistence-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
        let before = registry.snapshot().unwrap();
        registry
            .store
            .as_ref()
            .unwrap()
            .inject_failure_before_replace(true);

        assert!(
            registry
                .create_assistant_workspace(Some("Must rollback"), None, None)
                .is_err()
        );
        assert_eq!(registry.snapshot().unwrap(), before);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_custom_icon_persistence_does_not_publish_live_state() {
        let directory =
            std::env::temp_dir().join(format!("hh-icon-persistence-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let registry = SessionRegistry::persistent(directory.join("sessions.json")).unwrap();
        let before = registry.snapshot().unwrap();
        let workspace_id = before.workspaces[0].id;
        registry
            .store
            .as_ref()
            .unwrap()
            .inject_failure_before_replace(true);

        assert!(
            registry
                .set_workspace_custom_icon(
                    workspace_id,
                    Some("00000000-0000-4000-8000-000000000004.png".to_owned()),
                )
                .is_err()
        );
        assert_eq!(registry.snapshot().unwrap(), before);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stable_ssh_workstation_creation_is_delivered_to_the_rail_and_survives_restart() {
        let directory =
            std::env::temp_dir().join(format!("hh-ssh-workstation-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let before = registry.snapshot().unwrap();
        let (workspace_id, _) = registry
            .create_simulated_ssh_workspace(Some("Safe local simulation"), "test@local-host")
            .unwrap();

        let update = registry
            .pane_updates(Some(before.revision), &[], &[], true, 0)
            .unwrap();
        let delivered = update
            .snapshot
            .expect("new workstation snapshot is delivered");
        assert!(
            delivered
                .workspaces
                .iter()
                .any(|workspace| workspace.id == workspace_id)
        );
        let created = delivered
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .unwrap();
        assert_eq!(created.title, "Safe local simulation");
        assert!(matches!(
            created.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Connected,
            } if destination == "test@local-host"
        ));

        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let saved = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .expect("saved SSH workstation remains after restart");
        assert_eq!(saved.title, "Safe local simulation");
        assert!(matches!(
            saved.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Offline,
            } if destination == "test@local-host"
        ));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn confirmed_ssh_workstation_is_durable_before_session_attachment() {
        let directory =
            std::env::temp_dir().join(format!("hh-ssh-workstation-intent-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");
        let ids = SshWorkspaceIds {
            workspace: Uuid::new_v4(),
            tab: Uuid::new_v4(),
            pane: Uuid::new_v4(),
        };

        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        registry
            .persist_ssh_workspace_intent(
                Some("Durable before connection".to_owned()),
                "test@local-host",
                ids,
            )
            .unwrap();
        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let snapshot = recovered.snapshot().unwrap();
        let saved = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == ids.workspace)
            .expect("confirmed SSH workstation remains after a restart");
        assert_eq!(saved.title, "Durable before connection");
        assert_eq!(saved.active_terminal_count, 0);
        assert!(matches!(
            saved.connection,
            WorkspaceConnection::SystemSsh {
                ref destination,
                status: WorkspaceConnectionStatus::Offline,
            } if destination == "test@local-host"
        ));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_ssh_intent_does_not_create_or_replace_a_terminal() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();

        assert!(registry.connect_ssh(first, "-A").is_err());

        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.state().unwrap().1.len(), 1);
    }

    #[test]
    fn appearance_mutations_keep_global_defaults_and_entity_overrides_independent() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let pane_id = first_pane_id(&snapshot).unwrap();
        let terminal_default = AppearanceColor::new(0x95, 0xcc, 0x7f);
        let workspace_default = AppearanceColor::new(0xc9, 0x90, 0xe5);
        let terminal_override = AppearanceColor::new(0xef, 0x71, 0x7a);
        let workspace_override = AppearanceColor::new(0xe4, 0xbd, 0x72);

        registry
            .set_default_terminal_accent(terminal_default)
            .unwrap();
        registry
            .set_default_workspace_color(workspace_default)
            .unwrap();
        registry
            .set_pane_color(pane_id, Some(terminal_override))
            .unwrap();
        registry
            .set_workspace_color(workspace_id, Some(workspace_override))
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.appearance.default_terminal_accent,
            terminal_default
        );
        assert_eq!(
            snapshot.appearance.default_workspace_color,
            workspace_default
        );
        assert_eq!(snapshot.workspaces[0].color, Some(workspace_override));
        assert_eq!(
            find_pane_mut_in_snapshot(&mut snapshot.clone(), pane_id).and_then(|pane| pane.color),
            Some(terminal_override)
        );
        assert_eq!(snapshot.appearance.recent_colors[0], workspace_override);

        registry.set_pane_color(pane_id, None).unwrap();
        registry.set_workspace_color(workspace_id, None).unwrap();
        let reset = registry.snapshot().unwrap();
        assert_eq!(reset.workspaces[0].color, None);
        assert_eq!(
            find_pane_mut_in_snapshot(&mut reset.clone(), pane_id).and_then(|pane| pane.color),
            None
        );
        assert_eq!(reset.appearance.default_terminal_accent, terminal_default);
        assert_eq!(reset.appearance.default_workspace_color, workspace_default);
    }

    #[test]
    fn saved_workspace_management_renames_pins_reorders_and_deletes_deterministically() {
        let registry = SessionRegistry::new().unwrap();
        let first = registry.snapshot().unwrap().workspaces[0].id;
        let (second, _) = registry.create_workspace(Some("Second")).unwrap();
        let (third, _) = registry.create_workspace(Some("Third")).unwrap();

        registry.rename_workspace(second, "Build tools").unwrap();
        registry.set_workspace_pinned(second, true).unwrap();
        registry.set_workspace_pinned(third, true).unwrap();
        registry
            .move_pinned_workspace(third, WorkspacePinMove::Up)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let build = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == second)
            .unwrap();
        let third_workspace = snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == third)
            .unwrap();
        assert_eq!(build.title, "Build tools");
        assert!(build.pinned);
        assert!(third_workspace.pinned);
        assert_eq!(third_workspace.pin_order, 1);
        assert_eq!(build.pin_order, 2);

        registry.delete_workspace(second).unwrap();
        let snapshot = registry.snapshot().unwrap();
        assert!(
            snapshot
                .workspaces
                .iter()
                .all(|workspace| workspace.id != second)
        );
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == third)
                .unwrap()
                .pin_order,
            1
        );
        assert!(registry.delete_workspace(first).is_ok());
        assert!(registry.delete_workspace(third).is_err());
    }

    #[test]
    fn workspace_reorder_stays_in_its_group_and_persists_explicit_order() {
        let registry = SessionRegistry::new().unwrap();
        let first = registry.snapshot().unwrap().workspaces[0].id;
        let (second, _) = registry.create_workspace(Some("Second")).unwrap();
        let (third, _) = registry.create_workspace(Some("Third")).unwrap();

        registry.reorder_workspace(third, first, false).unwrap();
        registry.set_workspace_pinned(second, true).unwrap();
        assert!(registry.reorder_workspace(third, second, false).is_err());

        let snapshot = registry.snapshot().unwrap();
        let mut regular = snapshot
            .workspaces
            .iter()
            .filter(|workspace| !workspace.pinned)
            .map(|workspace| (workspace.title.as_str(), workspace.order))
            .collect::<Vec<_>>();
        regular.sort_by_key(|(_, order)| *order);
        assert_eq!(regular, vec![("Third", 1), ("Workstation 1", 2)]);
        assert!(
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == second)
                .is_some_and(|workspace| workspace.pinned)
        );
    }

    #[test]
    fn disconnect_keeps_saved_workspace_tabs_and_layout_offline() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let pane_id = first_pane_id(&snapshot).unwrap();
        let expected_panes = pane_ids_for_workspace(&snapshot.workspaces[0]);
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            };
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::SystemSsh {
                host: "build-node".to_owned(),
            };
        }

        registry.disconnect_workspace(workspace_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];

        assert_eq!(pane_ids_for_workspace(workspace), expected_panes);
        assert!(matches!(workspace.tabs[0].layout, PaneLayout::Leaf { .. }));
        assert_eq!(workspace.active_terminal_count, 0);
        assert_eq!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            }
        );
        assert!(registry.state.read().panes.contains_key(&pane_id));
    }

    #[test]
    fn closing_the_last_terminal_keeps_an_ssh_workstation_connected() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            };
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::SystemSsh {
                host: "build-node".to_owned(),
            };
        }

        registry.close_pane(pane_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace = &snapshot.workspaces[0];

        // Zero terminals is an empty workstation, not a disconnect: the next
        // terminal must open instead of demanding a reconnect.
        assert!(workspace.tabs.is_empty());
        assert_eq!(workspace.active_terminal_count, 0);
        assert_eq!(
            workspace.connection,
            WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Connected,
            }
        );
    }
}
