//! Remote workstation operations: tmux scans/attach and bounded remote directory listing.
use super::{
    RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry, TerminalRuntimePane,
    encode_desired_state,
};
use crate::layout::pane_ids_for_workspace;
use crate::process::{fallback_cwd, run_bounded_command};
use crate::pty::PtySession;
use crate::tmux::{
    parse_tmux_scan, plan_tmux_session_attachments, probe_error_summary, remote_directory_command,
    run_tmux_probe, tmux_local_probe_command, tmux_reports_no_server, tmux_ssh_probe_command,
};
use anyhow::{Context, Result, bail};
use hh_protocol::{
    MAX_PANES, Pane, PaneLayout, Tab, TerminalIdentity, TerminalIdentitySource, TerminalProfile,
    TmuxScanScope, TmuxSession, TmuxSessionAttachIssue, TmuxSessionId, WorkspaceConnection,
    WorkspaceConnectionStatus, validate_workspace_dir,
};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) const TMUX_SCAN_MIN_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) const REMOTE_LS_MIN_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) const REMOTE_LS_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) const MAX_REMOTE_DIRECTORY_ENTRIES: usize = 200;

#[derive(Debug, Eq, PartialEq)]
pub struct TmuxAttachmentResult {
    pub pane_ids: Vec<Uuid>,
    pub skipped: Vec<TmuxSessionAttachIssue>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TmuxScanResult {
    pub scope: TmuxScanScope,
    pub sessions: Vec<TmuxSession>,
    pub open_session_ids: Vec<TmuxSessionId>,
    pub no_server: bool,
}

#[derive(Debug, Default)]
pub(crate) struct TmuxScanGate {
    active: HashSet<Uuid>,
    last_completed: HashMap<Uuid, Instant>,
}

#[derive(Debug)]
pub(crate) struct TmuxScanPermit {
    gate: Arc<Mutex<TmuxScanGate>>,
    workspace_id: Uuid,
}

impl Drop for TmuxScanPermit {
    fn drop(&mut self) {
        let mut gate = self.gate.lock();
        gate.active.remove(&self.workspace_id);
        gate.last_completed
            .insert(self.workspace_id, Instant::now());
    }
}

#[derive(Debug, Default)]
pub(crate) struct RemoteLsGate {
    active: HashSet<Uuid>,
    last_completed: HashMap<Uuid, Instant>,
}

#[derive(Debug)]
pub(crate) struct RemoteLsPermit {
    gate: Arc<Mutex<RemoteLsGate>>,
    workspace_id: Uuid,
}

impl Drop for RemoteLsPermit {
    fn drop(&mut self) {
        let mut gate = self.gate.lock();
        gate.active.remove(&self.workspace_id);
        gate.last_completed
            .insert(self.workspace_id, Instant::now());
    }
}

impl SessionRegistry {
    pub(crate) fn begin_remote_ls(&self, workspace_id: Uuid) -> Result<RemoteLsPermit> {
        let gate = Arc::clone(&self.remote_ls_gate);
        {
            let mut state = gate.lock();
            if state.active.contains(&workspace_id) {
                bail!("a directory listing is already running for this workstation");
            }
            if state
                .last_completed
                .get(&workspace_id)
                .is_some_and(|completed| completed.elapsed() < REMOTE_LS_MIN_INTERVAL)
            {
                bail!("wait before listing remote directories again");
            }
            state.active.insert(workspace_id);
        }
        Ok(RemoteLsPermit { gate, workspace_id })
    }

    pub fn list_remote_directory(&self, workspace_id: Uuid, path: &str) -> Result<Vec<String>> {
        validate_workspace_dir(path).map_err(anyhow::Error::from)?;
        let _permit = self.begin_remote_ls(workspace_id)?;
        let mut entries = match self.workspace_connection(workspace_id)? {
            WorkspaceConnection::Local => {
                let mut entries = Vec::new();
                for entry in
                    std::fs::read_dir(path).with_context(|| format!("read directory {path}"))?
                {
                    let entry = entry.with_context(|| format!("read directory entry in {path}"))?;
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if !name.starts_with('.') && entry.file_type()?.is_dir() {
                        entries.push(name);
                    }
                }
                entries
            }
            WorkspaceConnection::SystemSsh { destination, .. } => {
                let output = run_bounded_command(
                    remote_directory_command(&destination, path)?,
                    REMOTE_LS_TIMEOUT,
                    "remote directory listing",
                )?;
                if !output.success {
                    let message = output.stderr.trim();
                    if message.is_empty() {
                        bail!("remote directory listing failed");
                    }
                    bail!("remote directory listing failed: {message}");
                }
                output
                    .stdout
                    .lines()
                    .filter_map(|line| line.strip_suffix('/'))
                    .filter(|name| !name.is_empty() && !name.starts_with('.'))
                    .map(str::to_owned)
                    .collect()
            }
        };
        entries.sort_unstable();
        entries.dedup();
        entries.truncate(MAX_REMOTE_DIRECTORY_ENTRIES);
        Ok(entries)
    }

    pub(crate) fn begin_tmux_scan(&self, workspace_id: Uuid) -> Result<TmuxScanPermit> {
        let gate = Arc::clone(&self.tmux_scan_gate);
        {
            let mut state = gate.lock();
            if state.active.contains(&workspace_id) {
                bail!("a tmux scan is already running for this workstation");
            }
            if state
                .last_completed
                .get(&workspace_id)
                .is_some_and(|completed| completed.elapsed() < TMUX_SCAN_MIN_INTERVAL)
            {
                bail!("wait before scanning tmux sessions again");
            }
            state.active.insert(workspace_id);
        }
        Ok(TmuxScanPermit { gate, workspace_id })
    }

    /// Performs an explicit bounded metadata-only scan of the default tmux
    /// server for one workstation. It never starts tmux, reconnects a saved
    /// SSH workstation, or writes scan output to terminal history.
    pub fn scan_tmux_sessions(&self, workspace_id: Uuid) -> Result<TmuxScanResult> {
        let _scan_permit = self.begin_tmux_scan(workspace_id)?;
        let connection = self.workspace_connection(workspace_id)?;
        let (scope, probe) = match connection {
            WorkspaceConnection::Local => (TmuxScanScope::Local, tmux_local_probe_command()?),
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => (
                TmuxScanScope::SystemSsh {
                    destination: destination.clone(),
                },
                tmux_ssh_probe_command(&destination)?,
            ),
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => bail!("reconnect this SSH workstation before scanning tmux sessions"),
        };
        let open_session_ids = self.open_tmux_session_ids(workspace_id)?;
        let output = run_tmux_probe(probe)?;
        if !output.success {
            if tmux_reports_no_server(&output.stderr) {
                return Ok(TmuxScanResult {
                    scope,
                    sessions: Vec::new(),
                    open_session_ids: Vec::new(),
                    no_server: true,
                });
            }
            bail!("tmux scan failed: {}", probe_error_summary(&output.stderr));
        }
        let sessions = parse_tmux_scan(&output.stdout)?;
        let open_session_ids = sessions
            .iter()
            .filter(|session| open_session_ids.contains(&session.id))
            .map(|session| session.id.clone())
            .collect();
        Ok(TmuxScanResult {
            scope,
            sessions,
            open_session_ids,
            no_server: false,
        })
    }

    /// Opens each selected existing tmux session in an independent live tab.
    /// Each target is isolated: an immediate attach failure returns a clear
    /// issue for that target while the rest can still open.
    pub fn attach_tmux_sessions(
        &self,
        workspace_id: Uuid,
        session_ids: &[TmuxSessionId],
    ) -> Result<TmuxAttachmentResult> {
        let connection = {
            let state = self.state.read();
            state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(|workspace| workspace.connection.clone())
                .with_context(|| format!("workstation {workspace_id} does not exist"))?
        };
        if matches!(
            connection,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            }
        ) {
            bail!("reconnect this SSH workstation before opening tmux");
        }
        let probe = match &connection {
            WorkspaceConnection::Local => tmux_local_probe_command()?,
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => tmux_ssh_probe_command(destination)?,
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => unreachable!("offline connection rejected above"),
        };
        let output = run_tmux_probe(probe)?;
        if !output.success {
            bail!("tmux scan failed: {}", probe_error_summary(&output.stderr));
        }
        let sessions = parse_tmux_scan(&output.stdout)?;
        let known_sessions = sessions
            .into_iter()
            .map(|session| (session.id.clone(), session))
            .collect::<HashMap<_, _>>();
        let already_open = self.open_tmux_session_ids(workspace_id)?;
        let plan = plan_tmux_session_attachments(session_ids, &already_open, &known_sessions)?;
        let mut result = TmuxAttachmentResult {
            pane_ids: Vec::new(),
            skipped: plan.skipped,
        };
        for session in plan.launch {
            match self.attach_tmux_session_one(workspace_id, &session, &connection) {
                Ok(pane_id) => result.pane_ids.push(pane_id),
                Err(error) => result.skipped.push(TmuxSessionAttachIssue {
                    session_id: session.id,
                    message: error.to_string(),
                }),
            }
        }
        Ok(result)
    }

    /// Sessions this workstation currently shows in a *live* tab. A tab whose
    /// attach died must not keep its session hostage: the picker has to offer
    /// it again so the user can reopen it.
    pub(crate) fn open_tmux_session_ids(
        &self,
        workspace_id: Uuid,
    ) -> Result<HashSet<TmuxSessionId>> {
        let state = self.state.read();
        let workspace = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        Ok(pane_ids_for_workspace(workspace)
            .into_iter()
            .filter_map(|pane_id| state.panes.get(&pane_id)?.terminal())
            .filter(|terminal| terminal.exit_status.is_none())
            .filter_map(|terminal| terminal.kind.tmux_session_id().cloned())
            .collect())
    }

    pub(crate) fn attach_tmux_session_one(
        &self,
        workspace_id: Uuid,
        tmux_session: &TmuxSession,
        connection: &WorkspaceConnection,
    ) -> Result<Uuid> {
        let pane_id = Uuid::new_v4();
        let (session, kind) = match connection {
            WorkspaceConnection::Local => (
                PtySession::spawn_tmux_local(
                    pane_id,
                    workspace_id,
                    &tmux_session.id,
                    &self.history,
                )?,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            ),
            WorkspaceConnection::SystemSsh {
                destination,
                status: WorkspaceConnectionStatus::Connected,
            } => (
                PtySession::spawn_tmux_ssh(
                    pane_id,
                    workspace_id,
                    destination,
                    &tmux_session.id,
                    &self.history,
                )?,
                RuntimePaneKind::TmuxSystemSsh {
                    host: destination.clone(),
                    session_id: tmux_session.id.clone(),
                },
            ),
            WorkspaceConnection::SystemSsh {
                status: WorkspaceConnectionStatus::Offline,
                ..
            } => bail!("reconnect this SSH workstation before opening tmux"),
        };
        self.register_live_tmux_tab(workspace_id, pane_id, tmux_session, &session, kind)
    }

    pub(crate) fn register_live_tmux_tab(
        &self,
        workspace_id: Uuid,
        pane_id: Uuid,
        tmux_session: &TmuxSession,
        session: &Arc<PtySession>,
        kind: RuntimePaneKind,
    ) -> Result<Uuid> {
        if let Err(error) = session.confirm_live_for_tmux_attach() {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let already_open = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .map(pane_ids_for_workspace)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pane_id| state.panes.get(&pane_id)?.terminal())
                .filter(|terminal| terminal.exit_status.is_none())
                .filter_map(|terminal| terminal.kind.tmux_session_id())
                .any(|existing| existing == &tmux_session.id);
            if already_open {
                bail!("already open in this workstation");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let still_connected = matches!(
                workspace.connection,
                WorkspaceConnection::Local
                    | WorkspaceConnection::SystemSsh {
                        status: WorkspaceConnectionStatus::Connected,
                        ..
                    }
            );
            if !still_connected {
                bail!("workstation went offline before opening tmux");
            }
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: tmux_session.name.clone(),
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf {
                    pane: Pane {
                        id: pane_id,
                        kind: hh_protocol::PaneKind::Terminal,
                        title: format!("tmux {}", tmux_session.name),
                        shell: "tmux".to_owned(),
                        color: None,
                        identity: TerminalIdentity {
                            profile: TerminalProfile::Tmux,
                            source: TerminalIdentitySource::Command,
                        },
                        status: hh_protocol::PaneStatus::default(),
                        custom_title: None,
                        profile_override: None,
                        custom_icon: None,
                    },
                },
            });
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(session),
                        last_valid_cwd: fallback_cwd()?,
                        kind,
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
            Ok(pane_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::layout::{first_pane_id, layout_contains};
    use crate::pty::PtySession;
    use crate::registry::identity::refresh_workspace_activity;
    use crate::registry::{SessionRegistry, create_owner_only_directory};
    use crate::tmux::{plan_tmux_session_attachments, tmux_session};
    use portable_pty::CommandBuilder;
    use std::ffi::OsString;
    use uuid::Uuid;

    #[test]
    fn live_tmux_runtime_tab_is_registered_and_selectable() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 1"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$9", "editor");

        let attached = registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();
        assert_eq!(attached, pane_id);
        registry.activate_tab(pane_id).unwrap();
        assert!(registry.pane_snapshot(pane_id).is_ok());
        let snapshot = registry.snapshot().unwrap();
        assert!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .any(|tab| layout_contains(&tab.layout, pane_id))
        );
    }

    #[test]
    fn a_plain_terminal_added_to_a_tmux_tab_survives_restart_without_the_tmux_pane() {
        let directory = std::env::temp_dir().join(format!("hh-tmux-group-test-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let tmux_pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            tmux_pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 1"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$11", "persisted-group");
        registry
            .register_live_tmux_tab(
                workspace_id,
                tmux_pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();

        let plain_pane_id = registry.create_group_terminal(tmux_pane_id).unwrap();
        let live_snapshot = registry.snapshot().unwrap();
        let live_tab = live_snapshot.workspaces[0]
            .tabs
            .iter()
            .find(|tab| layout_contains(&tab.layout, tmux_pane_id))
            .expect("tmux tab remains present while attached");
        assert!(matches!(
            &live_tab.layout,
            PaneLayout::Stack { panes, .. }
                if panes.iter().any(|pane| pane.id == tmux_pane_id)
                    && panes.iter().any(|pane| pane.id == plain_pane_id)
        ));

        drop(session);
        drop(registry);

        let recovered = SessionRegistry::persistent(&snapshot_path).unwrap();
        let recovered_snapshot = recovered.snapshot().unwrap();
        assert!(recovered_snapshot.workspaces[0].tabs.iter().any(|tab| {
            matches!(
                &tab.layout,
                PaneLayout::Leaf { pane } if pane.id == plain_pane_id
            )
        }));

        drop(recovered);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn immediate_tmux_attach_exit_never_registers_a_placeholder_tab() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![OsString::from("/usr/bin/false")]),
            "failed tmux fixture",
            &registry.history,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.exit_status().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "failed tmux fixture did not exit"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let tmux_session = tmux_session("$10", "logs");

        let error = registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("before the terminal became live")
        );
        assert!(registry.pane_snapshot(pane_id).is_err());
    }

    #[test]
    fn a_live_remote_tmux_tab_keeps_its_workstation_connected_and_a_dead_one_does_not() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        {
            let mut state = registry.state.write();
            state.snapshot.workspaces[0].connection = WorkspaceConnection::SystemSsh {
                destination: "build-node".to_owned(),
                status: WorkspaceConnectionStatus::Offline,
            };
            // Only a tmux attach remains, exactly what survives closing the
            // initial SSH tab.
            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .kind = RuntimePaneKind::TmuxSystemSsh {
                host: "build-node".to_owned(),
                session_id: TmuxSessionId::try_from("$3".to_owned()).unwrap(),
            };
            assert!(refresh_workspace_activity(&mut state));
            assert_eq!(
                state.snapshot.workspaces[0].connection,
                WorkspaceConnection::SystemSsh {
                    destination: "build-node".to_owned(),
                    status: WorkspaceConnectionStatus::Connected,
                }
            );

            state
                .panes
                .get_mut(&pane_id)
                .unwrap()
                .terminal_mut()
                .unwrap()
                .exit_status = Some("Exited with code 255".to_owned());
            assert!(refresh_workspace_activity(&mut state));
            assert_eq!(
                state.snapshot.workspaces[0].connection,
                WorkspaceConnection::SystemSsh {
                    destination: "build-node".to_owned(),
                    status: WorkspaceConnectionStatus::Offline,
                }
            );
        }
    }

    #[test]
    fn a_dead_tmux_tab_releases_its_session_back_to_the_picker() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let pane_id = Uuid::new_v4();
        let session = PtySession::spawn_command(
            pane_id,
            workspace_id,
            CommandBuilder::from_argv(vec![
                OsString::from("/bin/sh"),
                OsString::from("-c"),
                OsString::from("printf fixture; sleep 5"),
            ]),
            "live tmux fixture",
            &registry.history,
        )
        .unwrap();
        let tmux_session = tmux_session("$9", "editor");
        registry
            .register_live_tmux_tab(
                workspace_id,
                pane_id,
                &tmux_session,
                &session,
                RuntimePaneKind::TmuxLocal {
                    session_id: tmux_session.id.clone(),
                },
            )
            .unwrap();
        assert_eq!(
            registry.open_tmux_session_ids(workspace_id).unwrap(),
            HashSet::from([tmux_session.id.clone()])
        );

        registry
            .state
            .write()
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .terminal_mut()
            .unwrap()
            .exit_status = Some("Exited with code 255".to_owned());

        // The frozen tab must not reserve the session, or the picker offers
        // nothing to reopen after an SSH drop killed every attach.
        assert!(
            registry
                .open_tmux_session_ids(workspace_id)
                .unwrap()
                .is_empty()
        );
        let plan = plan_tmux_session_attachments(
            std::slice::from_ref(&tmux_session.id),
            &registry.open_tmux_session_ids(workspace_id).unwrap(),
            &HashMap::from([(tmux_session.id.clone(), tmux_session.clone())]),
        )
        .unwrap();
        assert_eq!(plan.launch, vec![tmux_session]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn tmux_scan_gate_rejects_concurrent_and_rapid_repeat_scans() {
        let registry = SessionRegistry::new().unwrap();
        let workspace_id = registry.snapshot().unwrap().workspaces[0].id;
        let permit = registry.begin_tmux_scan(workspace_id).unwrap();
        assert!(registry.begin_tmux_scan(workspace_id).is_err());
        drop(permit);
        assert!(registry.begin_tmux_scan(workspace_id).is_err());
    }
}
