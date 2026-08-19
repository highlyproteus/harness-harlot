//! Runtime identity discovery: process profiles, titles, and workspace activity.
use super::{RegistryState, RuntimePane};
use crate::history;
use crate::layout::{find_pane_in_snapshot, find_pane_mut_in_snapshot, pane_ids_for_workspace};
use crate::process::valid_local_cwd;
use hh_protocol::{
    NotificationKind, Pane, SessionSnapshot, TerminalIdentity, TerminalIdentitySource,
    TerminalProfile, WorkspaceConnection, WorkspaceConnectionStatus, terminal_profile_for_command,
    terminal_profile_for_executable, terminal_profile_for_title,
};
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use uuid::Uuid;

pub(crate) const IDENTITY_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) const MAX_DISCOVERY_PROCESSES: usize = 4_096;

pub(crate) const MAX_DISCOVERY_DESCENDANTS_PER_PANE: usize = 64;

pub(crate) const MAX_DISCOVERY_DEPTH: usize = 4;

pub(crate) fn refresh_process_metadata(shared: &Arc<RwLock<RegistryState>>, force: bool) {
    let started = Instant::now();
    let (inputs, discover_profiles) = {
        let state = shared.read();
        if !force
            && state.last_identity_refresh.is_some_and(|last| {
                started.saturating_duration_since(last) < IDENTITY_REFRESH_INTERVAL
            })
        {
            return;
        }
        let inputs = state
            .panes
            .iter()
            .filter_map(|(pane_id, runtime)| {
                let terminal = runtime.terminal()?;
                terminal
                    .session
                    .process_id()
                    .map(|process_id| (*pane_id, Pid::from_u32(process_id)))
            })
            .collect::<Vec<_>>();
        let discover_profiles = inputs.iter().any(|(pane_id, _)| {
            find_pane_in_snapshot(&state.snapshot, *pane_id)
                .is_some_and(|pane| pane.custom_title.is_none() && pane.profile_override.is_none())
        });
        (inputs, discover_profiles)
    };

    let process_ids = inputs.iter().map(|(_, pid)| *pid).collect::<Vec<_>>();
    let mut system = System::new();
    if !process_ids.is_empty() {
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&process_ids),
            ProcessRefreshKind::new().with_cwd(UpdateKind::Always),
        );
    }
    let cwd_by_pane = inputs
        .iter()
        .filter_map(|(pane_id, pid)| {
            system
                .process(*pid)
                .and_then(sysinfo::Process::cwd)
                .filter(|cwd| valid_local_cwd(cwd))
                .map(|cwd| (*pane_id, cwd.to_path_buf()))
        })
        .collect::<HashMap<_, _>>();
    let profiles = if discover_profiles {
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            ProcessRefreshKind::new().with_exe(UpdateKind::Always),
        );
        (system.processes().len() <= MAX_DISCOVERY_PROCESSES).then(|| {
            let children = process_children(&system);
            inputs
                .iter()
                .map(|(pane_id, pid)| {
                    (
                        *pane_id,
                        discover_descendant_profile(&system, &children, *pid),
                    )
                })
                .collect::<HashMap<_, _>>()
        })
    } else {
        None
    };

    let mut state = shared.write();
    for (pane_id, pid) in inputs {
        let Some(terminal) = state
            .panes
            .get_mut(&pane_id)
            .and_then(RuntimePane::terminal_mut)
        else {
            continue;
        };
        if !terminal.kind.is_local() || terminal.session.process_id() != Some(pid.as_u32()) {
            continue;
        }
        if let Some(cwd) = cwd_by_pane.get(&pane_id) {
            terminal.last_valid_cwd.clone_from(cwd);
        }
        if let Some(profiles) = &profiles {
            terminal.detected_command_profile = profiles.get(&pane_id).copied().flatten();
        }
    }
    state.last_identity_refresh = Some(started);
    refresh_runtime_metadata(&mut state);
}

pub(crate) fn refresh_runtime_metadata(state: &mut RegistryState) {
    let mut labels = Vec::new();
    for (pane_id, runtime) in &mut state.panes {
        let Some(runtime) = runtime.terminal_mut() else {
            continue;
        };
        let Ok(observed) = runtime.session.exit_status() else {
            continue;
        };
        if observed.is_some() && observed != runtime.exit_status {
            runtime.exit_status.clone_from(&observed);
            labels.push((
                *pane_id,
                runtime.recovered,
                observed,
                runtime.kind.shell_label(),
            ));
        }
    }
    if !labels.is_empty() {
        for (pane_id, recovered, status, shell_label) in labels {
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                recovered,
                status.as_deref(),
                &shell_label,
            );
            if status.is_some() {
                state.append_notification(
                    pane_id,
                    NotificationKind::Completed,
                    None,
                    history::now_ms(),
                );
            }
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    let identity_inputs = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            let terminal = runtime.terminal()?;
            terminal.kind.is_local().then(|| {
                (
                    *pane_id,
                    terminal.session.terminal_title(),
                    terminal.detected_command_profile,
                )
            })
        })
        .collect::<Vec<_>>();
    let mut identity_changed = false;
    for (pane_id, title_signal, command_profile) in identity_inputs {
        if let Some(pane) = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id) {
            identity_changed |=
                resolve_pane_identity(pane, title_signal.as_deref(), command_profile);
        }
    }
    if identity_changed {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    if refresh_workspace_activity(state) {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
}

/// Recomputes per-workstation terminal counts and SSH reachability.
///
/// An SSH workstation goes offline only when every remote pane it still owns
/// has died — a real transport failure. Deliberately closing terminals is not
/// a disconnect, so a workstation with zero tabs stays connected and its next
/// terminal simply opens.
pub(crate) fn refresh_workspace_activity(state: &mut RegistryState) -> bool {
    let workspace_activity = state
        .snapshot
        .workspaces
        .iter()
        .map(|workspace| {
            let mut active = 0_u32;
            let mut remote_panes = 0_u32;
            let mut remote_live = 0_u32;
            for pane_id in pane_ids_for_workspace(workspace) {
                let Some(runtime) = state.panes.get(&pane_id).and_then(RuntimePane::terminal)
                else {
                    continue;
                };
                let live = runtime.exit_status.is_none();
                if live {
                    active = active.saturating_add(1);
                }
                if runtime.kind.is_remote() {
                    remote_panes = remote_panes.saturating_add(1);
                    if live {
                        remote_live = remote_live.saturating_add(1);
                    }
                }
            }
            (workspace.id, active, remote_panes, remote_live)
        })
        .collect::<Vec<_>>();
    let mut workspace_changed = false;
    for workspace in &mut state.snapshot.workspaces {
        let Some((_, active, remote_panes, remote_live)) = workspace_activity
            .iter()
            .find(|(workspace_id, _, _, _)| *workspace_id == workspace.id)
        else {
            continue;
        };
        if workspace.active_terminal_count != *active {
            workspace.active_terminal_count = *active;
            workspace_changed = true;
        }
        if let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection {
            let next = if *remote_live > 0 {
                Some(WorkspaceConnectionStatus::Connected)
            } else if *remote_panes > 0 {
                Some(WorkspaceConnectionStatus::Offline)
            } else {
                None
            };
            if let Some(next) = next
                && *status != next
            {
                *status = next;
                workspace_changed = true;
            }
        }
    }
    workspace_changed
}

fn process_children(system: &System) -> HashMap<Pid, Vec<Pid>> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    children
}

pub(crate) fn discover_descendant_profile(
    system: &System,
    children: &HashMap<Pid, Vec<Pid>>,
    root: Pid,
) -> Option<TerminalProfile> {
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut inspected = 0_usize;
    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= MAX_DISCOVERY_DEPTH {
            continue;
        }
        for child in children.get(&parent).into_iter().flatten() {
            inspected += 1;
            if inspected > MAX_DISCOVERY_DESCENDANTS_PER_PANE {
                return None;
            }
            if let Some(process) = system.process(*child)
                && let Some(profile) = process
                    .name()
                    .to_str()
                    .and_then(terminal_profile_for_command)
                    .or_else(|| process.exe().and_then(terminal_profile_for_executable))
            {
                return Some(profile);
            }
            queue.push_back((*child, depth + 1));
        }
    }
    None
}

pub(crate) fn resolve_pane_identity(
    pane: &mut Pane,
    terminal_title: Option<&str>,
    command_profile: Option<TerminalProfile>,
) -> bool {
    let (profile, mut source, generated_title) = if let Some(profile) = pane.profile_override {
        (
            profile,
            TerminalIdentitySource::UserProfile,
            profile.display_name().to_owned(),
        )
    } else if let Some(profile) = terminal_title.and_then(terminal_profile_for_title) {
        (
            profile,
            TerminalIdentitySource::TerminalTitle,
            profile.display_name().to_owned(),
        )
    } else if let Some(profile) = command_profile {
        (
            profile,
            TerminalIdentitySource::Command,
            profile.display_name().to_owned(),
        )
    } else {
        let title = if pane.identity.source == TerminalIdentitySource::Fallback
            && pane.title.starts_with("Terminal")
        {
            pane.title.clone()
        } else {
            TerminalProfile::Terminal.display_name().to_owned()
        };
        (
            TerminalProfile::Terminal,
            TerminalIdentitySource::Fallback,
            title,
        )
    };
    let title = pane.custom_title.clone().unwrap_or(generated_title);
    if pane.custom_title.is_some() && source == TerminalIdentitySource::Fallback {
        source = TerminalIdentitySource::UserRename;
    }
    let identity = TerminalIdentity { profile, source };
    let changed = pane.identity != identity || pane.title != title;
    pane.identity = identity;
    pane.title = title;
    changed
}

pub(crate) fn set_pane_runtime_label(
    snapshot: &mut SessionSnapshot,
    pane_id: Uuid,
    recovered: bool,
    status: Option<&str>,
    shell_label: &str,
) {
    let label = match status {
        Some("terminating") => format!("{shell_label} · terminating"),
        Some(status) => format!("{shell_label} · exited ({status})"),
        None if recovered => format!("{shell_label} · recovered with a fresh shell"),
        None => shell_label.to_owned(),
    };
    if let Some(pane) = find_pane_mut_in_snapshot(snapshot, pane_id) {
        pane.shell = label;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::first_pane_id;
    use crate::registry::SessionRegistry;
    use uuid::Uuid;

    #[test]
    fn custom_name_and_selected_profile_resolve_independently() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.custom_title = Some("Release console".to_owned());
        pane.profile_override = Some(TerminalProfile::Hermes);

        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Release console");
        assert_eq!(pane.identity.profile, TerminalProfile::Hermes);
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.custom_title = None;
        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Hermes Agent");
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.profile_override = None;
        resolve_pane_identity(pane, Some("Claude Code"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), Some(TerminalProfile::Codex));
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), None);
        assert_eq!(pane.title, "Terminal");
        assert_eq!(pane.identity, TerminalIdentity::default());
    }

    #[test]
    fn custom_name_selected_profile_and_uploaded_icon_remain_independent() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        registry.rename_pane(pane_id, "My work").unwrap();
        registry
            .set_pane_profile(pane_id, Some(TerminalProfile::Claude))
            .unwrap();
        let icon = format!("{}.png", Uuid::new_v4());
        registry
            .set_pane_custom_icon(pane_id, Some(icon.clone()))
            .unwrap();
        registry.rename_pane(pane_id, "Release watch").unwrap();

        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.title, "Release watch");
        assert_eq!(pane.custom_title.as_deref(), Some("Release watch"));
        assert_eq!(pane.profile_override, Some(TerminalProfile::Claude));
        assert_eq!(pane.custom_icon.as_deref(), Some(icon.as_str()));
        assert_eq!(pane.identity.profile, TerminalProfile::Claude);

        registry.reset_pane_identity(pane_id).unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert_eq!(pane.custom_title, None);
        assert_eq!(pane.profile_override, None);
        assert_eq!(pane.custom_icon, None);
    }
}
