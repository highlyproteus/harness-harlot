//! Workstation lifecycle: creation, SSH intents, pins, order, and appearance defaults.
use super::{
    InitialTerminalSpawn, ProcessScan, RuntimePane, RuntimePaneBackend, RuntimePaneKind,
    SessionRegistry, SshWorkspaceIds, TerminalRuntimePane, encode_desired_state, ssh_pane_title,
};
use crate::layout::{find_pane_mut, pane_ids_for_workspace};
use crate::persistence::{MAX_RECENT_COLORS, MAX_TITLE_CHARS, MAX_WORKSPACES, validate_title};
use crate::process::{fallback_cwd, local_spawn_dir};
use crate::pty::PtySession;
use crate::registry::bots::{forget_bots, workstation_count};
use crate::registry::identity::{
    PANE_DISCONNECTED, refresh_workspace_activity, set_pane_runtime_label,
};
use crate::registry::panes::Reattached;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, MAX_PANES, MAX_WORKSTATION_DEPTH, Pane, PaneLayout, SessionSnapshot, Tab,
    TerminalIdentity, Workspace, WorkspaceConnection, WorkspaceConnectionStatus, WorkspaceKind,
    WorkspacePinMove, effective_working_dir, validate_ssh_host, validate_workspace_dir,
    workstation_depth, workstation_descendants,
};
use std::collections::HashSet;
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

/// Whether two workstations run on the same machine: both local, or both
/// reached over SSH at the same destination.
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

/// The connection of a new workstation nested in `parent`: the parent's
/// machine. Fails when `parent` cannot hold another nesting level.
fn nested_connection(workspaces: &[Workspace], parent: Uuid) -> Result<WorkspaceConnection> {
    let workspace = workspaces
        .iter()
        .find(|workspace| workspace.id == parent)
        .with_context(|| format!("workstation {parent} does not exist"))?;
    if workspace.is_bot() {
        bail!("a bot cannot hold nested workstations");
    }
    if workstation_depth(workspaces, parent).is_none_or(|depth| depth >= MAX_WORKSTATION_DEPTH) {
        bail!("workstations nest at most {MAX_WORKSTATION_DEPTH} levels deep");
    }
    Ok(workspace.connection.clone())
}

/// Default title of a workstation rooted at `dir`: the folder's name.
fn directory_title(dir: &str) -> Option<String> {
    dir.rsplit('/')
        .find(|component| !component.is_empty())
        .map(|name| name.chars().take(MAX_TITLE_CHARS).collect())
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
    pub(crate) fn ensure_workspace_accepts_workstation_tabs(
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
        if workspace.is_bot() {
            bail!("a bot only holds its threads; open a new thread instead");
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

    /// Creates a workstation and opens its first terminal in the effective
    /// root folder. With `parent` it is nested there and runs on the parent's
    /// machine (for a remote parent, over SSH to the same destination);
    /// otherwise it is a top-level local workstation.
    pub fn create_workspace(
        &self,
        title: Option<&str>,
        parent: Option<Uuid>,
        working_dir: Option<String>,
    ) -> Result<(Uuid, Uuid)> {
        let title = normalize_workspace_title(title)?;
        if let Some(dir) = working_dir.as_deref() {
            validate_workspace_dir(dir).map_err(anyhow::Error::from)?;
        }
        let (connection, root) = {
            let state = self.state.read();
            if workstation_count(&state.snapshot) >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let connection = match parent {
                Some(parent) => nested_connection(&state.snapshot.workspaces, parent)?,
                None => WorkspaceConnection::Local,
            };
            let root = working_dir.clone().or_else(|| {
                parent.and_then(|parent| {
                    effective_working_dir(&state.snapshot.workspaces, parent).map(str::to_owned)
                })
            });
            (connection, root)
        };
        let workspace_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let cwd = match connection {
            WorkspaceConnection::Local => local_spawn_dir(root.as_deref())?,
            WorkspaceConnection::SystemSsh { .. } => fallback_cwd()?,
        };
        let InitialTerminalSpawn {
            session,
            kind,
            tab_title,
            ..
        } = self.spawn_initial_workspace_terminal(
            pane_id,
            workspace_id,
            &connection,
            root.as_deref(),
            &cwd,
        )?;
        let result = (|| {
            let mut state = self.state.write();
            if workstation_count(&state.snapshot) >= MAX_WORKSPACES {
                bail!("workstation limit of {MAX_WORKSPACES} reached");
            }
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            if let Some(parent) = parent {
                nested_connection(&state.snapshot.workspaces, parent)?;
            }
            let number = workstation_count(&state.snapshot) + 1;
            let order = next_workspace_order(&state.snapshot.workspaces, false);
            let pane = state.new_runtime_pane(pane_id, &cwd, &kind);
            let connection = match connection {
                WorkspaceConnection::Local => WorkspaceConnection::Local,
                WorkspaceConnection::SystemSsh { destination, .. } => {
                    WorkspaceConnection::SystemSsh {
                        destination,
                        status: WorkspaceConnectionStatus::Connected,
                    }
                }
            };
            state.snapshot.workspaces.push(Workspace {
                id: workspace_id,
                title: title
                    .or_else(|| working_dir.as_deref().and_then(directory_title))
                    .unwrap_or_else(|| format!("Workstation {number}")),
                color: None,
                pinned: false,
                pin_order: 0,
                order,
                active_terminal_count: 1,
                connection,
                working_dir,
                kind: WorkspaceKind::Workstation,
                parent_workstation: parent,
                home: false,
                instructions: None,
                owner_bot: None,
                custom_icon: None,
                bot: None,
                tabs: vec![Tab {
                    owner_thread: None,
                    id: Uuid::new_v4(),
                    title: tab_title,
                    custom_title: None,
                    color: None,
                    custom_icon: None,
                    pinned: false,
                    owner_bot: None,
                    layout: PaneLayout::Leaf { pane },
                }],
            });
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
        let session = self.spawn_ssh_transport(ids.pane, ids.workspace, destination, None)?;
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
        if workstation_count(&state.snapshot) >= MAX_WORKSPACES {
            bail!("workstation limit of {MAX_WORKSPACES} reached");
        }
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        let pane = Pane {
            id: ids.pane,
            kind: hh_protocol::PaneKind::Terminal,
            title: ssh_pane_title(destination),
            shell: "ssh".to_owned(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            status_changed_at_ms: 0,
            custom_title: None,
            profile_override: None,
            custom_icon: None,
            unseen: false,
            progress: None,
        };
        state.snapshot.workspaces.push(Workspace {
            id: ids.workspace,
            title: title.unwrap_or_else(|| destination.to_owned()),
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
            parent_workstation: None,
            home: false,
            instructions: None,
            owner_bot: None,
            custom_icon: None,
            bot: None,
            tabs: vec![Tab {
                owner_thread: None,
                id: ids.tab,
                title: "Remote".to_owned(),
                custom_title: None,
                color: None,
                custom_icon: None,
                pinned: false,
                owner_bot: None,
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
                    process_scan: ProcessScan::Unknown,
                    omp_title_status: None,
                    title_baseline_pending: false,
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
        let session = PtySession::spawn_local(ids.pane, ids.workspace, None, &cwd)?;
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
        let parent = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| workspace.parent_workstation)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        let mut pinned = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.pinned && workspace.parent_workstation == parent)
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
        let source = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .map(|workspace| {
                (
                    workspace.pinned,
                    workspace.kind,
                    workspace.parent_workstation,
                )
            })
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        let target = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == target_workspace_id)
            .map(|workspace| {
                (
                    workspace.pinned,
                    workspace.kind,
                    workspace.parent_workstation,
                )
            })
            .with_context(|| format!("workstation {target_workspace_id} does not exist"))?;
        if source != target {
            bail!("workstations can only be reordered among their siblings");
        }
        let mut ordered = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| {
                (
                    workspace.pinned,
                    workspace.kind,
                    workspace.parent_workstation,
                ) == source
            })
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

    /// Disconnects a remote workstation and every workstation nested in it:
    /// their SSH sessions end and their saved tabs stay as offline panes.
    pub fn disconnect_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let targets = {
            let mut state = self.state.write();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. }) {
                bail!("only a system-SSH workstation can be disconnected");
            }
            let targets = std::iter::once(workspace_id)
                .chain(workstation_descendants(
                    &state.snapshot.workspaces,
                    workspace_id,
                ))
                .filter_map(|id| {
                    let workspace = state
                        .snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == id)?;
                    if !matches!(workspace.connection, WorkspaceConnection::SystemSsh { .. }) {
                        return None;
                    }
                    let sessions = pane_ids_for_workspace(workspace)
                        .into_iter()
                        .filter_map(|pane_id| {
                            let terminal = state.panes.get(&pane_id)?.terminal()?;
                            matches!(terminal.kind, RuntimePaneKind::SystemSsh { .. })
                                .then(|| (pane_id, Arc::clone(&terminal.session)))
                        })
                        .collect::<Vec<_>>();
                    Some((id, sessions))
                })
                .collect::<Vec<_>>();
            // Marked before their SSH clients stop, under the same lock: the
            // runtime refresh must see a disconnect, never an exit to report.
            for (pane_id, _) in targets.iter().flat_map(|(_, sessions)| sessions) {
                if let Ok(runtime) = state.terminal_pane_mut(*pane_id) {
                    runtime.exit_status = Some(PANE_DISCONNECTED.to_owned());
                }
                set_pane_runtime_label(
                    &mut state.snapshot,
                    *pane_id,
                    false,
                    Some(PANE_DISCONNECTED),
                    "system OpenSSH",
                );
            }
            targets
        };
        // The windows keep running on the host; Reconnect reattaches them.
        for (_, session) in targets.iter().flat_map(|(_, sessions)| sessions) {
            let _ = session.detach(PANE_DISCONNECTED);
        }
        for (id, _) in &targets {
            self.close_remote_clients(*id);
        }

        let mut state = self.state.write();
        if !state
            .snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            bail!("workstation {workspace_id} disappeared while disconnecting");
        }
        refresh_workspace_activity(&mut state);
        for (id, _) in &targets {
            if let Some(workspace) = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == *id)
                && let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection
            {
                *status = WorkspaceConnectionStatus::Offline;
            }
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)
        }
    }

    /// Reconnects an offline remote workstation and every offline workstation
    /// nested in it. Saved remote panes respawn in their workstation's
    /// effective root folder; a remote workstation without tabs gains a
    /// fresh terminal, nested ones are only marked connected.
    pub fn reconnect_workspace(&self, workspace_id: Uuid) -> Result<Uuid> {
        let pane_id = self
            .reconnect_one(workspace_id, true)?
            .context("reconnected workstation has no terminal")?;
        let nested = {
            let state = self.state.read();
            workstation_descendants(&state.snapshot.workspaces, workspace_id)
                .into_iter()
                .filter(|id| {
                    state.snapshot.workspaces.iter().any(|workspace| {
                        workspace.id == *id
                            && matches!(
                                workspace.connection,
                                WorkspaceConnection::SystemSsh {
                                    status: WorkspaceConnectionStatus::Offline,
                                    ..
                                }
                            )
                    })
                })
                .collect::<Vec<_>>()
        };
        for id in nested {
            self.reconnect_one(id, false)
                .with_context(|| format!("reconnect nested workstation {id}"))?;
        }
        Ok(pane_id)
    }

    fn reconnect_one(&self, workspace_id: Uuid, open_if_empty: bool) -> Result<Option<Uuid>> {
        let plan = self.reconnection_plan(workspace_id)?;
        validate_ssh_host(&plan.destination).map_err(anyhow::Error::from)?;
        let created_layout = open_if_empty && plan.pane_ids.is_empty();
        let mut pane_ids = plan.pane_ids;
        if created_layout {
            pane_ids.push(Uuid::new_v4());
        }
        // Reattaches every window still running on the host, with its
        // scrollback; a host that needs a prompt gets a sign-in tab first.
        let sessions = self.spawn_ssh_sessions(
            workspace_id,
            &plan.destination,
            plan.working_dir.as_deref(),
            &pane_ids,
        )?;
        let result = self.apply_workspace_reconnection(
            workspace_id,
            &plan.destination,
            created_layout,
            &pane_ids,
            &sessions,
        );
        if result.is_err() {
            for (_, session, _) in sessions {
                let _ = session.detach("reconnect failed");
            }
        }
        result.map(|()| pane_ids.first().copied())
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
            working_dir: effective_working_dir(&state.snapshot.workspaces, workspace_id)
                .map(str::to_owned),
            pane_ids,
        })
    }

    /// Publishes respawned SSH sessions into the desired state and marks the
    /// workstation connected again. A pane that reattached its still-running
    /// remote window keeps its status and progress.
    fn apply_workspace_reconnection(
        &self,
        workspace_id: Uuid,
        destination: &str,
        created_layout: bool,
        pane_ids: &[Uuid],
        sessions: &[(Uuid, Arc<PtySession>, Reattached)],
    ) -> Result<()> {
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
                title: ssh_pane_title(destination),
                shell: "ssh".to_owned(),
                color: None,
                identity: TerminalIdentity::default(),
                status: hh_protocol::PaneStatus::default(),
                status_changed_at_ms: 0,
                custom_title: None,
                profile_override: None,
                custom_icon: None,
                unseen: false,
                progress: None,
            };
            workspace.tabs.push(Tab {
                owner_thread: None,
                id: Uuid::new_v4(),
                title: "Remote".to_owned(),
                custom_title: None,
                color: None,
                custom_icon: None,
                pinned: false,
                owner_bot: None,
                layout: PaneLayout::Leaf { pane },
            });
        } else {
            for pane_id in pane_ids {
                if let Some(pane) = workspace
                    .tabs
                    .iter_mut()
                    .find_map(|tab| find_pane_mut(&mut tab.layout, *pane_id))
                {
                    pane.title = ssh_pane_title(destination);
                    "ssh".clone_into(&mut pane.shell);
                }
            }
        }
        *status = WorkspaceConnectionStatus::Connected;
        workspace.active_terminal_count = workspace
            .active_terminal_count
            .saturating_add(u32::try_from(sessions.len()).unwrap_or(u32::MAX));

        let mut replaced = Vec::new();
        for (pane_id, session, behind) in sessions {
            if state.terminal_pane(*pane_id).is_ok() {
                replaced.push(state.install_reattached_session(
                    *pane_id,
                    Arc::clone(session),
                    *behind,
                )?);
                continue;
            }
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
                        process_scan: ProcessScan::Unknown,
                        omp_title_status: None,
                        title_baseline_pending: *behind == Reattached::RunningProgram,
                    }),
                },
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(replaced);
        self.write_snapshot(&bytes)
    }

    /// Deletes a workstation and every workstation nested in it: ends their
    /// terminals, removes their galleries and kills their tmux sessions. The
    /// home workstation is refused.
    pub fn delete_workspace(&self, workspace_id: Uuid) -> Result<()> {
        let (removed_ids, pane_ids, sessions, tmux_clients, remote_hosts) = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if workspace.home {
                bail!("the home workstation cannot be deleted");
            }
            let removed_ids = std::iter::once(workspace_id)
                .chain(workstation_descendants(
                    &state.snapshot.workspaces,
                    workspace_id,
                ))
                .collect::<HashSet<_>>();
            let pane_ids = state
                .snapshot
                .workspaces
                .iter()
                .filter(|workspace| removed_ids.contains(&workspace.id))
                .flat_map(pane_ids_for_workspace)
                .collect::<Vec<_>>();
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
            let tmux_clients = removed_ids
                .iter()
                .filter_map(|id| state.tmux_clients.get(id).cloned())
                .collect::<Vec<_>>();
            // Every host each removed workstation ran tmux on: its own
            // destination and those of its direct SSH tabs.
            let remote_hosts = state
                .snapshot
                .workspaces
                .iter()
                .filter(|workspace| removed_ids.contains(&workspace.id))
                .map(|workspace| {
                    let mut hosts = pane_ids_for_workspace(workspace)
                        .iter()
                        .filter_map(|pane_id| state.panes.get(pane_id)?.terminal())
                        .filter_map(|terminal| match &terminal.kind {
                            RuntimePaneKind::SystemSsh { host } => Some(host.clone()),
                            _ => None,
                        })
                        .collect::<HashSet<_>>();
                    if let WorkspaceConnection::SystemSsh { destination, .. } =
                        &workspace.connection
                    {
                        hosts.insert(destination.clone());
                    }
                    (workspace.id, hosts)
                })
                .collect::<Vec<_>>();
            (removed_ids, pane_ids, sessions, tmux_clients, remote_hosts)
        };
        for session in &sessions {
            let _ = session.terminate_and_wait();
        }
        for (id, hosts) in &remote_hosts {
            self.kill_remote_sessions(*id, hosts);
        }
        let mut cleanup_errors: Vec<anyhow::Error> = Vec::new();
        for id in &removed_ids {
            if let Some(directory) = hh_protocol::gallery_directory(*id)
                && let Err(error) = std::fs::remove_dir_all(&directory)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                cleanup_errors.push(
                    anyhow::Error::new(error)
                        .context(format!("remove gallery directory {}", directory.display())),
                );
            }
        }
        for client in tmux_clients {
            let _ = client.kill_session();
        }
        let mut state = self.state.write();
        if !state
            .snapshot
            .workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            bail!("workstation {workspace_id} disappeared while deleting");
        }
        let removed_bots = state
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| removed_ids.contains(&workspace.id) && workspace.is_bot())
            .map(|workspace| workspace.id)
            .collect::<HashSet<_>>();
        state
            .snapshot
            .workspaces
            .retain(|workspace| !removed_ids.contains(&workspace.id));
        forget_bots(
            &mut state.snapshot,
            &removed_bots,
            self.bots_dir().ok().as_deref(),
        );
        let removed = pane_ids
            .into_iter()
            .filter_map(|pane_id| state.panes.remove(&pane_id))
            .collect::<Vec<_>>();
        for id in &removed_ids {
            state.tmux_clients.remove(id);
            state.tmux_sinks.remove(id);
        }
        normalize_workspace_orders(&mut state.snapshot.workspaces);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(removed);
        self.write_snapshot(&bytes)?;
        if let Some(error) = cleanup_errors.into_iter().next() {
            return Err(error).context("clean up workstation resources");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "workspaces_tests.rs"]
mod tests;
