//! Runtime identity discovery: process profiles, titles, and workspace activity.
use super::{PaneLocation, ProcessScan, RegistryState, RuntimePane, StatusNotice, ssh_pane_title};
use crate::layout::{find_pane_in_snapshot, find_pane_mut_in_snapshot, pane_ids_for_workspace};
use crate::process::valid_local_cwd;
use crate::registry::status::omp_title_status;
use hh_protocol::{
    Pane, PaneStatus, ProgressSource, SessionSnapshot, TerminalIdentity, TerminalIdentitySource,
    TerminalProfile, WorkspaceConnection, WorkspaceConnectionStatus,
    terminal_profile_for_arguments, terminal_profile_for_command, terminal_profile_for_executable,
    terminal_profile_for_title,
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
        // A pane with progress keeps discovery running even when every pane
        // is renamed or pinned: the progress rule needs the detected agent.
        let discover_profiles = inputs.iter().any(|(pane_id, _)| {
            find_pane_in_snapshot(&state.snapshot, *pane_id).is_some_and(|pane| {
                (pane.custom_title.is_none() && pane.profile_override.is_none())
                    || pane.progress.is_some()
            })
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
                        discover_descendant_profile(&mut system, &children, *pid),
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
            terminal.process_scan = profiles
                .get(&pane_id)
                .copied()
                .unwrap_or(ProcessScan::Unknown);
        }
    }
    state.last_identity_refresh = Some(started);
    refresh_runtime_metadata(&mut state);
}

/// Exit reason of a saved tmux window recovery could not reattach
/// (`Transport::Unattached`, reported as `not reattached: <why>`). Its
/// program is still running in the window.
pub(crate) const PANE_NOT_REATTACHED_PREFIX: &str = "not reattached:";
/// Exit reason of a remote tmux pane whose SSH connection dropped. Its
/// program keeps running on the host.
pub(crate) const PANE_CONNECTION_LOST: &str = "connection lost";
/// Exit reason of a pane whose SSH workstation the user disconnected.
pub(crate) const PANE_DISCONNECTED: &str = "disconnected";
/// Exit reason of a pane whose shell HH is replacing (a bot restart).
pub(crate) const PANE_RESTARTING: &str = "restarting";

/// How a terminal's transport ended, from the reason it reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PaneEnd<'a> {
    /// The pane's program exited: its turn is over.
    Exited(&'a str),
    /// HH lost or let go of the pane while its program may still run: after
    /// a failed reattach, a lost SSH connection, a disconnect or a restart.
    /// Not news: only the pane's label changes.
    Detached(&'a str),
}

impl<'a> PaneEnd<'a> {
    /// The one classifier of exit reasons; every detached reason above is
    /// recognised here and nowhere else.
    pub(crate) fn classify(reason: &'a str) -> Self {
        if reason.starts_with(PANE_NOT_REATTACHED_PREFIX)
            || matches!(
                reason,
                PANE_CONNECTION_LOST | PANE_DISCONNECTED | PANE_RESTARTING
            )
        {
            Self::Detached(reason)
        } else {
            Self::Exited(reason)
        }
    }
}

/// How an omp title status observed by the refresh applies to the pane.
enum TitleStatus {
    /// The first title after a reattach: where the program already was.
    Baseline(PaneStatus),
    Changed(PaneStatus),
}

pub(crate) fn refresh_runtime_metadata(state: &mut RegistryState) {
    let mut ends = Vec::new();
    for (pane_id, runtime) in &mut state.panes {
        let Some(runtime) = runtime.terminal_mut() else {
            continue;
        };
        // A detached end holds until the pane is reattached: the later exit
        // of its transport (the SSH client a disconnect stopped, the shell a
        // restart replaces) is not its program's.
        if runtime
            .exit_status
            .as_deref()
            .is_some_and(|reason| matches!(PaneEnd::classify(reason), PaneEnd::Detached(_)))
        {
            continue;
        }
        let Ok(Some(observed)) = runtime.session.exit_status() else {
            continue;
        };
        if runtime.exit_status.as_ref() != Some(&observed) {
            runtime.exit_status = Some(observed.clone());
            ends.push((
                *pane_id,
                runtime.recovered,
                observed,
                runtime.kind.shell_label(),
            ));
        }
    }
    if !ends.is_empty() {
        for (pane_id, recovered, reason, shell_label) in ends {
            set_pane_runtime_label(
                &mut state.snapshot,
                pane_id,
                recovered,
                Some(&reason),
                &shell_label,
            );
            if let PaneEnd::Exited(_) = PaneEnd::classify(&reason) {
                state.clear_pane_progress(pane_id);
                state.update_pane_status(
                    pane_id,
                    PaneStatus::Done,
                    StatusNotice::Always(None),
                    crate::now_ms(),
                );
            }
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    // Local and SSH terminals: an agent's title reaches HH through ssh as
    // well, so remote tabs get the same identity and status tracking.
    let identity_inputs = state
        .panes
        .iter()
        .filter_map(|(pane_id, runtime)| {
            let terminal = runtime.terminal()?;
            (terminal.kind.is_local() || terminal.kind.is_remote()).then(|| {
                (
                    *pane_id,
                    terminal.session.terminal_title(),
                    terminal.process_scan,
                    terminal.location(),
                )
            })
        })
        .collect::<Vec<_>>();
    let mut identity_changed = false;
    for (pane_id, title_signal, process_scan, location) in identity_inputs {
        let title_detected = title_signal.as_deref().and_then(title_profile);
        // What actually runs in the pane, never the profile a user or a bot
        // pinned: the program the title names, else what the process scan
        // found, where a finished scan without an agent means the shell.
        let detected_profile = title_detected.or(match process_scan {
            ProcessScan::Agent(profile) => Some(profile),
            ProcessScan::NoAgent => Some(TerminalProfile::Terminal),
            ProcessScan::Unknown => None,
        });
        let resolved = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id).map(|pane| {
            identity_changed |= resolve_pane_identity(
                pane,
                title_signal.as_deref(),
                process_scan.agent(),
                Some(&location),
            );
            // Progress belongs to the agent that reported it; once another
            // program is detected in the pane, it is stale. Nothing
            // detected is no evidence either way.
            if let Some(detected) = detected_profile
                && pane
                    .progress
                    .as_ref()
                    .is_some_and(|progress| progress_profile(progress.source) != detected)
            {
                pane.progress = None;
                identity_changed = true;
            }
            (pane.identity.profile, pane.status)
        });
        let resolved_profile = resolved.map(|(profile, _)| profile);
        let current_status = resolved.map(|(_, status)| status);
        let status_update = state
            .panes
            .get_mut(&pane_id)
            .and_then(RuntimePane::terminal_mut)
            .and_then(|runtime| {
                if resolved_profile != Some(TerminalProfile::Omp) {
                    runtime.omp_title_status = None;
                    return None;
                }
                let Some(next) = title_signal.as_deref().and_then(omp_title_status) else {
                    runtime.omp_title_status = None;
                    return None;
                };
                if runtime.omp_title_status == Some(next) {
                    return None;
                }
                let previous = runtime.omp_title_status.replace(next);
                if std::mem::take(&mut runtime.title_baseline_pending) {
                    return Some(TitleStatus::Baseline(next));
                }
                Some(TitleStatus::Changed(
                    if previous == Some(PaneStatus::Working) && next == PaneStatus::Idle {
                        PaneStatus::Done
                    } else {
                        next
                    },
                ))
            });
        match status_update {
            Some(TitleStatus::Baseline(status)) => {
                state.update_pane_status(pane_id, status, StatusNotice::Silent, crate::now_ms());
            }
            // A finished turn stays Done until the next one starts: the idle
            // prompt after it, or a re-read once the tracker resets, is not news.
            Some(TitleStatus::Changed(status))
                if !(status == PaneStatus::Idle && current_status == Some(PaneStatus::Done)) =>
            {
                state.set_pane_status(pane_id, status);
            }
            Some(TitleStatus::Changed(_)) | None => {}
        }
    }
    if identity_changed {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
    if refresh_workspace_activity(state) {
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
    }
}

/// The terminal profile of the agent that reports progress from `source`.
fn progress_profile(source: ProgressSource) -> TerminalProfile {
    match source {
        ProgressSource::Omp => TerminalProfile::Omp,
        ProgressSource::Claude => TerminalProfile::Claude,
        ProgressSource::Codex => TerminalProfile::Codex,
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

/// Breadth-first over the pane's descendants: a process is identified by its
/// name, then its executable's install location, and only then by its command
/// line, which is read for that one process on demand.
pub(crate) fn discover_descendant_profile(
    system: &mut System,
    children: &HashMap<Pid, Vec<Pid>>,
    root: Pid,
) -> ProcessScan {
    let mut queue = VecDeque::from([(root, 0_usize)]);
    let mut inspected = 0_usize;
    while let Some((parent, depth)) = queue.pop_front() {
        if depth >= MAX_DISCOVERY_DEPTH {
            continue;
        }
        for child in children.get(&parent).into_iter().flatten() {
            inspected += 1;
            if inspected > MAX_DISCOVERY_DESCENDANTS_PER_PANE {
                return ProcessScan::Unknown;
            }
            let profile = system.process(*child).and_then(|process| {
                process
                    .name()
                    .to_str()
                    .and_then(terminal_profile_for_command)
                    .or_else(|| process.exe().and_then(terminal_profile_for_executable))
            });
            let profile = profile.or_else(|| {
                system.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[*child]),
                    ProcessRefreshKind::new().with_cmd(UpdateKind::Always),
                );
                system
                    .process(*child)
                    .and_then(|process| terminal_profile_for_arguments(process.cmd()))
            });
            if let Some(profile) = profile {
                return ProcessScan::Agent(profile);
            }
            queue.push_back((*child, depth + 1));
        }
    }
    ProcessScan::NoAgent
}

/// The agent a terminal title names: a known program title, or omp's live
/// `π <state> label` title, the only omp signal an SSH pane carries.
fn title_profile(title: &str) -> Option<TerminalProfile> {
    terminal_profile_for_title(title)
        .or_else(|| omp_title_status(title).map(|_| TerminalProfile::Omp))
}

pub(crate) fn resolve_pane_identity(
    pane: &mut Pane,
    terminal_title: Option<&str>,
    command_profile: Option<TerminalProfile>,
    location: Option<&PaneLocation>,
) -> bool {
    let (profile, mut source, generated_title) = if let Some(profile) = pane.profile_override {
        (
            profile,
            TerminalIdentitySource::UserProfile,
            profile.display_name().to_owned(),
        )
    } else if let Some(profile) = terminal_title.and_then(title_profile) {
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
        let local_name = match location {
            Some(PaneLocation::Local(cwd)) => cwd.file_name(),
            Some(PaneLocation::Remote(_)) | None => None,
        };
        let title = if let Some(name) = local_name {
            name.to_string_lossy().into_owned()
        } else if let Some(PaneLocation::Remote(host)) = location {
            ssh_pane_title(host)
        } else if pane.identity.source == TerminalIdentitySource::Fallback
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
    let label = match status.map(PaneEnd::classify) {
        Some(PaneEnd::Exited("terminating")) => format!("{shell_label} · terminating"),
        Some(PaneEnd::Exited(status)) => format!("{shell_label} · exited ({status})"),
        Some(PaneEnd::Detached(reason)) => format!("{shell_label} · {reason}"),
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
    fn only_reasons_where_the_program_may_still_run_classify_as_detached() {
        for reason in [
            "not reattached: tmux window @4 is missing",
            "connection lost",
            "disconnected",
            "restarting",
        ] {
            assert_eq!(PaneEnd::classify(reason), PaneEnd::Detached(reason));
        }
        for reason in [
            "Exited with code 0",
            "Exited with code 255",
            "tmux control client exited",
            "connection lost later",
        ] {
            assert_eq!(PaneEnd::classify(reason), PaneEnd::Exited(reason));
        }
    }

    #[test]
    fn custom_name_and_selected_profile_resolve_independently() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.custom_title = Some("Release console".to_owned());
        pane.profile_override = Some(TerminalProfile::Hermes);

        resolve_pane_identity(
            pane,
            Some("Claude Code"),
            Some(TerminalProfile::Codex),
            None,
        );
        assert_eq!(pane.title, "Release console");
        assert_eq!(pane.identity.profile, TerminalProfile::Hermes);
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.custom_title = None;
        resolve_pane_identity(
            pane,
            Some("Claude Code"),
            Some(TerminalProfile::Codex),
            None,
        );
        assert_eq!(pane.title, "Hermes Agent");
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserProfile);

        pane.profile_override = None;
        resolve_pane_identity(
            pane,
            Some("Claude Code"),
            Some(TerminalProfile::Codex),
            None,
        );
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), Some(TerminalProfile::Codex), None);
        assert_eq!(pane.title, "Codex CLI");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Command);

        resolve_pane_identity(pane, Some("editor"), None, None);
        assert_eq!(pane.title, "Terminal");
        assert_eq!(pane.identity, TerminalIdentity::default());
    }

    #[test]
    fn fallback_title_uses_local_folder_or_ssh_host_and_custom_title_wins() {
        let mut snapshot = SessionSnapshot::seeded();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let pane = find_pane_mut_in_snapshot(&mut snapshot, pane_id).unwrap();
        pane.custom_title = None;
        pane.profile_override = None;
        let local = PaneLocation::Local("/Users/x/Projects/hh-ui-web".into());

        resolve_pane_identity(pane, Some("editor"), None, Some(&local));
        assert_eq!(pane.title, "hh-ui-web");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Fallback);

        // A remote shell is named for its host, never this machine's folder,
        // and loses an agent's name once the agent's title is gone.
        let remote = PaneLocation::Remote("devbox".to_owned());
        resolve_pane_identity(pane, Some("π ⠋ fixing tests"), None, Some(&remote));
        assert_eq!(pane.identity.profile, TerminalProfile::Omp);
        resolve_pane_identity(pane, Some("user@devbox: ~"), None, Some(&remote));
        assert_eq!(pane.title, "SSH devbox");
        assert_eq!(pane.identity.source, TerminalIdentitySource::Fallback);

        pane.custom_title = Some("My work".to_owned());
        resolve_pane_identity(pane, Some("editor"), None, Some(&local));
        assert_eq!(pane.title, "My work");
        assert_eq!(pane.identity.source, TerminalIdentitySource::UserRename);
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
