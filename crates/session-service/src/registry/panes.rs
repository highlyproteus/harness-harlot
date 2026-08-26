//! Pane lifecycle: creation, input, identity overrides, and close/reattach.
use super::{
    InitialTerminalSpawn, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    TerminalRuntimePane, encode_desired_state,
};
use crate::layout::{
    add_tab, detach_pane, find_pane_in_snapshot, find_pane_mut_in_snapshot, layout_contains,
    split_layout, workspace_id_for_pane,
};
use crate::persistence;
use crate::persistence::{MAX_TABS_PER_WORKSPACE, MAX_TITLE_CHARS, validate_title};
use crate::process::{fallback_cwd, local_spawn_dir, shell_title};
use crate::pty::PtySession;
use crate::registry::identity::{
    refresh_workspace_activity, resolve_pane_identity, set_pane_runtime_label,
};
use crate::registry::workspaces::remember_recent_color;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    AppearanceColor, MAX_PANES, Pane, PaneKind, PaneLayout, PaneStatus, SplitAxis, Tab,
    TerminalIdentity, TerminalModifiers, TerminalMouseAction, TerminalMouseButton, TerminalPoint,
    TerminalProfile, TerminalSelectionKind, WorkspaceConnection, WorkspaceConnectionStatus,
    normalize_browser_url, normalize_browser_url_or_default, validate_ssh_host,
};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn browser_title(url: &str, title: Option<&str>) -> String {
    let explicit = title
        .map(|title| {
            title
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>()
        })
        .map(|title| title.trim().chars().take(MAX_TITLE_CHARS).collect())
        .filter(|title: &String| !title.is_empty());
    explicit
        .or_else(|| {
            url::Url::parse(url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
        })
        .unwrap_or_else(|| "Browser".to_owned())
}

impl SessionRegistry {
    pub fn create_pane(&self, target_pane: Uuid, axis: SplitAxis) -> Result<Uuid> {
        {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.require_terminal_layout_pane(target_pane)?;
        }
        let new_id = Uuid::new_v4();
        let cwd = self.cwd_for_pane(target_pane)?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let (session, kind) = self.spawn_pane_for_workspace(new_id, workspace_id, &cwd, None)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let mut new_pane = state.new_pane(new_id, Some(cwd.as_path()));
            if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
                "ssh".clone_into(&mut new_pane.shell);
            }
            let did_split = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .any(|tab| split_layout(&mut tab.layout, target_pane, new_pane.clone(), axis))
            });
            if !did_split {
                bail!("target pane {target_pane} does not exist");
            }
            state.panes.insert(
                new_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok(new_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn create_group_terminal(&self, target_pane: Uuid) -> Result<Uuid> {
        let (workspace_id, project_dir) = {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.require_terminal_layout_pane(target_pane)?;
            let (workspace, tab) = state
                .snapshot
                .workspaces
                .iter()
                .find_map(|workspace| {
                    workspace.tabs.iter().find_map(|tab| {
                        layout_contains(&tab.layout, target_pane).then_some((workspace, tab))
                    })
                })
                .with_context(|| format!("target pane {target_pane} does not exist"))?;
            let project_dir = tab.project_dir.clone().or_else(|| {
                tab.parent_tab.and_then(|parent_id| {
                    workspace
                        .tabs
                        .iter()
                        .find(|parent| parent.id == parent_id)
                        .and_then(|parent| parent.project_dir.clone())
                })
            });
            (workspace.id, project_dir)
        };
        let new_id = Uuid::new_v4();
        let cwd = match project_dir.as_deref() {
            Some(dir) => local_spawn_dir(Some(dir))?,
            None => self.cwd_for_pane(target_pane)?,
        };
        let (session, kind) =
            self.spawn_pane_for_workspace(new_id, workspace_id, &cwd, project_dir.as_deref())?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let mut pane = state.new_pane(new_id, Some(cwd.as_path()));
            if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
                "ssh".clone_into(&mut pane.shell);
            }
            let did_add = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone()))
            });
            if !did_add {
                bail!("target pane {target_pane} does not exist");
            }
            state.panes.insert(
                new_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            );
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
            Ok(new_id)
        })();
        if result.is_err() {
            let _ = session.terminate_and_wait();
        }
        result
    }

    pub fn create_group_browser(&self, target_pane: Uuid, url: Option<&str>) -> Result<Uuid> {
        let url = normalize_browser_url_or_default(url)?;
        let title = browser_title(&url, None);
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .find(|tab| layout_contains(&tab.layout, target_pane))
            })
            .with_context(|| format!("target pane {target_pane} does not exist"))?;
        let pane = Pane {
            id: pane_id,
            kind: PaneKind::Browser { url },
            title,
            shell: String::new(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        if !add_tab(&mut tab.layout, target_pane, pane) {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Browser,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub(crate) fn spawn_initial_workspace_terminal(
        &self,
        pane_id: Uuid,
        workspace_id: Uuid,
        connection: &WorkspaceConnection,
        working_dir: Option<&str>,
        cwd: &Path,
    ) -> Result<InitialTerminalSpawn> {
        match connection {
            WorkspaceConnection::Local => Ok(InitialTerminalSpawn {
                session: PtySession::spawn_local(pane_id, workspace_id, cwd, &self.history)?,
                kind: RuntimePaneKind::Local,
                pane_title: "Terminal 1".to_owned(),
                pane_shell: shell_title(),
                tab_title: "Terminals".to_owned(),
            }),
            WorkspaceConnection::SystemSsh { destination, .. } => Ok(InitialTerminalSpawn {
                session: PtySession::spawn_ssh(
                    pane_id,
                    workspace_id,
                    destination,
                    working_dir,
                    &self.history,
                )?,
                kind: RuntimePaneKind::SystemSsh {
                    host: destination.clone(),
                },
                pane_title: format!("SSH {destination}"),
                pane_shell: "ssh".to_owned(),
                tab_title: "Remote".to_owned(),
            }),
        }
    }

    /// Opens the sole initial terminal in a deliberately empty saved workspace.
    /// This request is rejected once any layout exists, so a repeated click or
    /// retried request cannot create duplicate terminals.
    pub fn create_workspace_terminal(&self, workspace_id: Uuid) -> Result<Uuid> {
        let (connection, working_dir) = {
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
            if !workspace.tabs.is_empty() {
                bail!("workstation {workspace_id} already has a terminal layout");
            }
            (workspace.connection.clone(), workspace.working_dir.clone())
        };

        let pane_id = Uuid::new_v4();
        let cwd = local_spawn_dir(working_dir.as_deref())?;
        let InitialTerminalSpawn {
            session,
            kind,
            pane_title,
            pane_shell,
            tab_title,
        } = self.spawn_initial_workspace_terminal(
            pane_id,
            workspace_id,
            &connection,
            working_dir.as_deref(),
            &cwd,
        )?;

        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if !workspace.tabs.is_empty() {
                bail!("workstation {workspace_id} already has a terminal layout");
            }
            let pane = Pane {
                id: pane_id,
                kind: hh_protocol::PaneKind::Terminal,
                title: pane_title,
                shell: pane_shell,
                color: None,
                identity: TerminalIdentity::default(),
                status: hh_protocol::PaneStatus::default(),
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            workspace.tabs.push(Tab {
                id: Uuid::new_v4(),
                title: tab_title,
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                layout: PaneLayout::Leaf { pane },
            });
            workspace.active_terminal_count = 1;
            if let WorkspaceConnection::SystemSsh { status, .. } = &mut workspace.connection {
                *status = WorkspaceConnectionStatus::Connected;
            }
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

    /// Starts the installed OpenSSH client only for an explicit, validated
    /// destination and places it in the target pane's tab strip.
    pub fn connect_ssh(&self, target_pane: Uuid, host: &str) -> Result<Uuid> {
        validate_ssh_host(host).map_err(anyhow::Error::from)?;
        {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.terminal_pane(target_pane)?;
            if !state
                .snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .any(|tab| layout_contains(&tab.layout, target_pane))
            {
                bail!("target pane {target_pane} does not exist");
            }
        }

        let pane_id = Uuid::new_v4();
        let cwd = fallback_cwd()?;
        let workspace_id = self.workspace_for_pane(target_pane)?;
        let session = PtySession::spawn_ssh(pane_id, workspace_id, host, None, &self.history)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let pane = Pane {
                id: pane_id,
                kind: hh_protocol::PaneKind::Terminal,
                title: format!("SSH {host}"),
                shell: "ssh".to_owned(),
                color: None,
                identity: TerminalIdentity::default(),
                status: hh_protocol::PaneStatus::default(),
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            let did_add = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone()))
            });
            if !did_add {
                bail!("target pane {target_pane} does not exist");
            }
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd,
                        kind: RuntimePaneKind::SystemSsh {
                            host: host.to_owned(),
                        },
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
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

    pub fn create_browser_tab(&self, workspace_id: Uuid, url: Option<&str>) -> Result<Uuid> {
        let url = normalize_browser_url_or_default(url)?;
        let title = browser_title(&url, None);
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("workstation tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        workspace.tabs.push(Tab {
            id: Uuid::new_v4(),
            title: title.clone(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: pane_id,
                    kind: PaneKind::Browser { url },
                    title,
                    shell: String::new(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    status: hh_protocol::PaneStatus::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Browser,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub fn create_assistant_tab(&self, workspace_id: Uuid) -> Result<Uuid> {
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace = state
            .snapshot
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == workspace_id)
            .with_context(|| format!("workstation {workspace_id} does not exist"))?;
        if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
            bail!("workstation tab limit of {MAX_TABS_PER_WORKSPACE} reached");
        }
        workspace.tabs.push(Tab {
            id: Uuid::new_v4(),
            title: "Assistant".to_owned(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: pane_id,
                    kind: PaneKind::Assistant,
                    title: "Assistant".to_owned(),
                    shell: String::new(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    status: hh_protocol::PaneStatus::default(),
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Assistant,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub fn create_group_assistant(&self, target_pane: Uuid) -> Result<Uuid> {
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .find(|tab| layout_contains(&tab.layout, target_pane))
            })
            .with_context(|| format!("target pane {target_pane} does not exist"))?;
        let pane = Pane {
            id: pane_id,
            kind: PaneKind::Assistant,
            title: "Assistant".to_owned(),
            shell: String::new(),
            color: None,
            identity: TerminalIdentity::default(),
            status: hh_protocol::PaneStatus::default(),
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        if !add_tab(&mut tab.layout, target_pane, pane) {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Assistant,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub fn set_browser_state(&self, pane_id: Uuid, url: &str, title: Option<&str>) -> Result<()> {
        let url = normalize_browser_url(url)?;
        let title = browser_title(&url, title);
        let mut state = self.state.write();
        match state.panes.get(&pane_id) {
            Some(RuntimePane {
                backend: RuntimePaneBackend::Browser,
            }) => {}
            Some(_) => bail!("pane {pane_id} is a terminal, not a browser"),
            None => bail!("pane {pane_id} does not exist"),
        }
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.title == title
            && matches!(&pane.kind, PaneKind::Browser { url: current } if current == &url)
        {
            return Ok(());
        }
        pane.kind = PaneKind::Browser { url };
        pane.title = title;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn rename_pane(&self, pane_id: Uuid, title: &str) -> Result<()> {
        let title = title.trim();
        validate_title(title, "terminal")?;
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.custom_title = Some(title.to_owned());
        title.clone_into(&mut pane.title);
        state.snapshot.revision += 1;
        {
            let bytes = encode_desired_state(&state)?;
            drop(state);
            self.write_snapshot(&bytes)?;
        };
        Ok(())
    }

    pub fn set_pane_profile(&self, pane_id: Uuid, profile: Option<TerminalProfile>) -> Result<()> {
        let mut state = self.state.write();
        let terminal_identity = match state.panes.get(&pane_id) {
            Some(runtime) => runtime.terminal().map(|terminal| {
                (
                    terminal.session.terminal_title(),
                    terminal.detected_command_profile,
                    terminal.last_valid_cwd.clone(),
                )
            }),
            None => bail!("pane {pane_id} does not exist"),
        };
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.custom_title.is_none() {
            pane.custom_title = Some(pane.title.clone());
        }
        pane.profile_override = profile;
        pane.custom_icon = None;
        if let Some((title_signal, command_profile, cwd)) = terminal_identity {
            resolve_pane_identity(
                pane,
                title_signal.as_deref(),
                command_profile,
                Some(cwd.as_path()),
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn set_pane_custom_icon(&self, pane_id: Uuid, icon: Option<String>) -> Result<()> {
        if let Some(icon) = icon.as_deref() {
            persistence::validate_custom_icon_id(icon)?;
        }
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.custom_title.is_none() {
            pane.custom_title = Some(pane.title.clone());
        }
        pane.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn reset_pane_identity(&self, pane_id: Uuid) -> Result<()> {
        let mut state = self.state.write();
        let terminal_identity = match state.panes.get(&pane_id) {
            Some(runtime) => runtime.terminal().map(|terminal| {
                (
                    terminal.session.terminal_title(),
                    terminal.detected_command_profile,
                    terminal.last_valid_cwd.clone(),
                )
            }),
            None => bail!("pane {pane_id} does not exist"),
        };
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.custom_title = None;
        pane.profile_override = None;
        pane.custom_icon = None;
        if let Some((title_signal, command_profile, cwd)) = terminal_identity {
            resolve_pane_identity(
                pane,
                title_signal.as_deref(),
                command_profile,
                Some(cwd.as_path()),
            );
        }
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)
    }

    pub fn close_pane(&self, pane_id: Uuid) -> Result<()> {
        let (session, was_terminal) = {
            let mut state = self.state.write();
            let pane_exists = state.snapshot.workspaces.iter().any(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| layout_contains(&tab.layout, pane_id))
            });
            if !pane_exists {
                bail!("pane {pane_id} does not exist");
            }
            let was_terminal = find_pane_in_snapshot(&state.snapshot, pane_id)
                .is_some_and(|pane| matches!(pane.kind, PaneKind::Terminal));
            let runtime = state.panes.get(&pane_id);
            let session = runtime
                .and_then(RuntimePane::terminal)
                .map(|terminal| Arc::clone(&terminal.session));
            let shell_label = runtime
                .and_then(RuntimePane::terminal)
                .map(|terminal| terminal.kind.shell_label());
            if let Some(shell_label) = shell_label {
                set_pane_runtime_label(
                    &mut state.snapshot,
                    pane_id,
                    false,
                    Some("terminating"),
                    &shell_label,
                );
                state.snapshot.revision = state.snapshot.revision.saturating_add(1);
                let bytes = encode_desired_state(&state)?;
                drop(state);
                self.write_snapshot(&bytes)?;
            }
            (session, was_terminal)
        };
        if let Some(session) = session {
            session.terminate_and_wait()?;
        }

        let mut state = self.state.write();
        let mut did_close = false;
        for workspace in &mut state.snapshot.workspaces {
            let Some(tab_index) = workspace
                .tabs
                .iter()
                .position(|tab| layout_contains(&tab.layout, pane_id))
            else {
                continue;
            };
            let (_, remaining) = detach_pane(workspace.tabs[tab_index].layout.clone(), pane_id);
            if let Some(remaining) = remaining {
                workspace.tabs[tab_index].layout = remaining;
            } else {
                let removed_tab = workspace.tabs.remove(tab_index).id;
                for tab in &mut workspace.tabs {
                    if tab.parent_tab == Some(removed_tab) {
                        tab.parent_tab = None;
                    }
                }
            }
            if was_terminal {
                workspace.active_terminal_count = workspace.active_terminal_count.saturating_sub(1);
            }
            did_close = true;
            break;
        }
        if !did_close {
            bail!("pane {pane_id} disappeared while closing");
        }
        let removed = state.panes.remove(&pane_id);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(removed);
        self.write_snapshot(&bytes)
    }

    /// Respawns one pane whose process exited, in place, keeping its tab,
    /// layout position, and pane ID.
    ///
    /// This is the recovery path for a transport that died under a live tab —
    /// an SSH drop leaves `tmux attach` dead with a frozen screen that ignores
    /// input. A tmux pane is re-attached with the same plain `attach-session`,
    /// so nothing is ever created or changed on the user's tmux server. A tmux
    /// session that no longer exists fails here instead of registering a fake
    /// live tab.
    pub fn reattach_pane(&self, pane_id: Uuid) -> Result<()> {
        let (kind, cwd, workspace_id) = {
            let state = self.state.read();
            let runtime = state.terminal_pane(pane_id)?;
            if runtime.exit_status.is_none() {
                bail!("this terminal is still live");
            }
            let workspace_id = workspace_id_for_pane(&state.snapshot, pane_id)
                .with_context(|| format!("pane {pane_id} has no workstation"))?;
            (
                runtime.kind.clone(),
                runtime.last_valid_cwd.clone(),
                workspace_id,
            )
        };
        let session = match &kind {
            RuntimePaneKind::Local => {
                PtySession::spawn_local(pane_id, workspace_id, &cwd, &self.history)?
            }
            RuntimePaneKind::SystemSsh { host } => {
                PtySession::spawn_ssh(pane_id, workspace_id, host, None, &self.history)?
            }
            RuntimePaneKind::TmuxLocal { session_id } => {
                PtySession::spawn_tmux_local(pane_id, workspace_id, session_id, &self.history)?
            }
            RuntimePaneKind::TmuxSystemSsh { host, session_id } => {
                PtySession::spawn_tmux_ssh(pane_id, workspace_id, host, session_id, &self.history)?
            }
        };
        if kind.is_runtime_only()
            && let Err(error) = session.confirm_live_for_tmux_attach()
        {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        let mut state = self.state.write();
        let runtime = state.terminal_pane_mut(pane_id)?;
        let previous = std::mem::replace(&mut runtime.session, session);
        runtime.exit_status = None;
        runtime.recovered = false;
        runtime.omp_title_status = None;
        let shell_label = kind.shell_label();
        set_pane_runtime_label(&mut state.snapshot, pane_id, false, None, &shell_label);
        state.set_pane_status(pane_id, PaneStatus::Idle);
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        // Terminate the previous transport only after releasing the state
        // lock: teardown performs bounded thread joins and must never block
        // the registry (mirrors `close_pane`).
        let _ = previous.terminate_and_wait();
        drop(previous);
        self.write_snapshot(&bytes)
    }

    pub fn set_pane_color(&self, pane_id: Uuid, color: Option<AppearanceColor>) -> Result<()> {
        let mut state = self.state.write();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        pane.color = color;
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

    pub fn write_input(&self, pane_id: Uuid, bytes: &[u8]) -> Result<()> {
        self.pane(pane_id)?.write_input(bytes)?;
        let mut state = self.state.write();
        if find_pane_in_snapshot(&state.snapshot, pane_id).is_some_and(|pane| {
            matches!(
                pane.status,
                PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention
            )
        }) {
            state.set_pane_status(pane_id, PaneStatus::Working);
        }
        Ok(())
    }

    pub fn begin_selection(
        &self,
        pane_id: Uuid,
        point: TerminalPoint,
        kind: TerminalSelectionKind,
    ) -> Result<()> {
        self.pane(pane_id)?.begin_selection(point, kind);
        Ok(())
    }

    pub fn update_selection(&self, pane_id: Uuid, point: TerminalPoint) -> Result<()> {
        self.pane(pane_id)?.update_selection(point);
        Ok(())
    }

    pub fn clear_selection(&self, pane_id: Uuid) -> Result<()> {
        self.pane(pane_id)?.clear_selection();
        Ok(())
    }

    pub fn selected_text(&self, pane_id: Uuid) -> Result<Option<String>> {
        Ok(self.pane(pane_id)?.selected_text())
    }

    pub fn scroll_pane(&self, pane_id: Uuid, lines: i32) -> Result<()> {
        self.pane(pane_id)?.scroll(lines);
        Ok(())
    }

    pub fn search_pane(&self, pane_id: Uuid, query: &str, forward: bool) -> Result<bool> {
        self.pane(pane_id)?.search_literal(query, forward)
    }

    pub fn mouse_input(
        &self,
        pane_id: Uuid,
        point: TerminalPoint,
        button: TerminalMouseButton,
        action: TerminalMouseAction,
        modifiers: TerminalModifiers,
    ) -> Result<()> {
        self.pane(pane_id)?
            .mouse_input(point, button, action, modifiers)
    }

    pub fn resize_pane(&self, pane_id: Uuid, columns: u16, rows: u16) -> Result<()> {
        self.pane(pane_id)?.resize(columns, rows)
    }

    pub fn pane_process_id(&self, pane_id: Uuid) -> Result<Option<u32>> {
        Ok(self.pane(pane_id)?.process_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{
        find_pane_in_snapshot, first_pane_id, first_pane_in_layout, pane_ids_for_workspace,
        pane_in_layout,
    };
    use crate::pty::{TEST_LOCAL_SSH_SEAM_ENABLED, validate_terminal_dimensions};
    use crate::registry::{SessionRegistry, create_owner_only_directory};
    use hh_protocol::DropPlacement;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn ssh_test_seam_honors_workspace_directory_and_keeps_direct_tabs_offline() {
        let directory = std::env::temp_dir().join(format!("hh-ssh-working-dir-{}", Uuid::new_v4()));
        create_owner_only_directory(&directory);
        TEST_LOCAL_SSH_SEAM_ENABLED.store(true, Ordering::Relaxed);

        let snapshot_path = directory.join("sessions.json");
        let registry = SessionRegistry::persistent(&snapshot_path).unwrap();
        let local_pane = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let direct_ssh_pane = registry
            .connect_ssh(local_pane, "admin@second-host")
            .unwrap();
        let long_host = "a".repeat(hh_protocol::MAX_SSH_HOST_LEN);
        let long_ssh_pane = registry.connect_ssh(local_pane, &long_host).unwrap();
        let (workspace_id, _) = registry
            .create_ssh_workspace(Some("SSH"), "admin@test-host")
            .unwrap();
        registry
            .set_workspace_working_dir(workspace_id, Some(directory.to_string_lossy().into_owned()))
            .unwrap();
        let pane_id = registry.create_workspace_tab(workspace_id).unwrap();
        let state = registry.state.read();
        assert_eq!(
            state.terminal_pane(pane_id).unwrap().last_valid_cwd,
            directory
        );
        drop(state);
        drop(registry);

        let recovered = SessionRegistry::persistent(snapshot_path).unwrap();
        let recovered_snapshot = recovered.snapshot().unwrap();
        let recovered_pane = find_pane_in_snapshot(&recovered_snapshot, direct_ssh_pane).unwrap();
        assert_eq!(
            recovered_pane.title,
            "SSH admin@second-host — Offline; reconnect required"
        );
        assert!(recovered.pane_process_id(direct_ssh_pane).is_err());
        recovered.close_pane(direct_ssh_pane).unwrap();
        assert!(find_pane_in_snapshot(&recovered.snapshot().unwrap(), direct_ssh_pane).is_none());
        let long_pane = find_pane_in_snapshot(&recovered_snapshot, long_ssh_pane).unwrap();
        assert!(long_pane.title.ends_with(" — Offline; reconnect required"));
        assert!(long_pane.title.chars().count() <= MAX_TITLE_CHARS);
        recovered.close_pane(long_ssh_pane).unwrap();
        drop(recovered);

        TEST_LOCAL_SSH_SEAM_ENABLED.store(false, Ordering::Relaxed);
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn reattach_respawns_an_exited_pane_in_place_and_refuses_a_live_one() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let pane_id = first_pane_id(&snapshot).unwrap();
        let tab_ids = snapshot.workspaces[0]
            .tabs
            .iter()
            .map(|tab| tab.id)
            .collect::<Vec<_>>();

        let error = registry.reattach_pane(pane_id).unwrap_err();
        assert!(error.to_string().contains("still live"));

        let dead_session = {
            let mut state = registry.state.write();
            let dead_session = {
                let terminal = state
                    .panes
                    .get_mut(&pane_id)
                    .unwrap()
                    .terminal_mut()
                    .unwrap();
                terminal.exit_status = Some("Exited with code 255".to_owned());
                terminal.omp_title_status = Some(PaneStatus::Done);
                Arc::clone(&terminal.session)
            };
            state.set_pane_status(pane_id, PaneStatus::Done);
            dead_session
        };
        dead_session.terminate_and_wait().unwrap();

        registry.reattach_pane(pane_id).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(
            snapshot.workspaces[0]
                .tabs
                .iter()
                .map(|tab| tab.id)
                .collect::<Vec<_>>(),
            tab_ids
        );
        assert!(pane_ids_for_workspace(&snapshot.workspaces[0]).contains(&pane_id));
        assert_eq!(
            registry.state.read().panes.get(&pane_id).map(|runtime| {
                runtime
                    .terminal()
                    .and_then(|terminal| terminal.exit_status.clone())
            }),
            Some(None)
        );
        let pane = find_pane_in_snapshot(&snapshot, pane_id).unwrap();
        assert!(!pane.shell.contains("exited"), "shell: {}", pane.shell);
        assert_eq!(pane.status, PaneStatus::Idle);
        assert_eq!(
            registry
                .state
                .read()
                .panes
                .get(&pane_id)
                .and_then(RuntimePane::terminal)
                .and_then(|terminal| terminal.omp_title_status),
            None
        );
        registry
            .write_input(pane_id, b"printf 'REATTACHED\\n'\r")
            .unwrap();
    }

    #[test]
    fn resize_propagates_the_exact_requested_grid_to_the_terminal_model() {
        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();

        registry.resize_pane(pane_id, 13, 3).unwrap();

        let (_, screens) = registry.state().unwrap();
        let screen = screens
            .iter()
            .find(|screen| screen.pane_id == pane_id)
            .unwrap();
        assert_eq!((screen.columns, screen.rows), (13, 3));
    }

    #[test]
    fn split_creates_a_second_live_shell_without_replacing_the_first() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();

        assert_ne!(first, second);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert!(registry.pane_process_id(second).unwrap().is_some());
        assert_eq!(registry.state().unwrap().1.len(), 2);
    }

    #[test]
    fn rearrange_swaps_layout_positions_without_restarting_shells() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry.swap_panes(first, second).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let layout = &snapshot.workspaces[0].tabs[0].layout;
        let PaneLayout::Split {
            first: left,
            second: right,
            ..
        } = layout
        else {
            panic!("expected split layout");
        };

        assert_eq!(first_pane_in_layout(left), second);
        assert_eq!(first_pane_in_layout(right), first);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
    }

    #[test]
    fn resize_bounds_reject_oom_dimensions_without_killing_sessions() {
        assert!(validate_terminal_dimensions(1_200, 500).is_ok());
        assert!(validate_terminal_dimensions(2_000, 301).is_err());
        assert!(validate_terminal_dimensions(1, 30).is_err());

        let registry = SessionRegistry::new().unwrap();
        let pane_id = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        assert!(registry.resize_pane(pane_id, u16::MAX, u16::MAX).is_err());
        assert!(registry.pane_process_id(pane_id).unwrap().is_some());
    }

    #[test]
    fn terminals_receive_human_names_and_can_be_renamed() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_group_terminal(first).unwrap();
        registry.rename_pane(second, "Build logs").unwrap();

        // Panes spawn at the fallback cwd ($HOME), so their default titles
        // are that directory's folder name rather than "Terminal N".
        let home_folder = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .and_then(|home| home.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "Terminal 1".to_owned());
        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Stack { panes, .. } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected a pane-local tab stack");
        };
        assert_eq!(panes[0].title, home_folder);
        assert_eq!(panes[1].title, "Build logs");
        assert_eq!(panes[1].shell, shell_title());
    }

    #[test]
    fn moving_a_live_tab_to_a_directional_split_preserves_its_process() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_group_terminal(first).unwrap();
        let first_pid = registry.pane_process_id(first).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry
            .move_pane_to_split(second, first, DropPlacement::Left)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected moved tab to become a split");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert_eq!(first_pane_in_layout(left), second);
        assert_eq!(first_pane_in_layout(right), first);
        assert_eq!(registry.pane_process_id(first).unwrap(), first_pid);
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
    }

    #[test]
    fn browser_moves_from_a_top_level_tab_into_a_terminal_split() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let terminal = first_pane_id(&snapshot).unwrap();
        let terminal_pid = registry.pane_process_id(terminal).unwrap();
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry
            .move_pane_to_split(browser, terminal, DropPlacement::Right)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected the browser to join the terminal split");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert_eq!(first_pane_in_layout(left), terminal);
        assert!(matches!(
            &**right,
            PaneLayout::Leaf { pane }
                if pane.id == browser && matches!(pane.kind, PaneKind::Browser { .. })
        ));
        assert_eq!(registry.pane_process_id(terminal).unwrap(), terminal_pid);
    }

    #[test]
    fn browser_moves_from_a_top_level_tab_into_a_terminal_tab_strip() {
        let registry = SessionRegistry::new().unwrap();
        let snapshot = registry.snapshot().unwrap();
        let workspace_id = snapshot.workspaces[0].id;
        let terminal = first_pane_id(&snapshot).unwrap();
        let browser = registry
            .create_browser_tab(workspace_id, Some("https://example.com"))
            .unwrap();

        registry.move_pane_to_tab(browser, terminal).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(snapshot.workspaces[0].tabs.len(), 1);
        let PaneLayout::Stack { panes, active } = &snapshot.workspaces[0].tabs[0].layout else {
            panic!("expected the browser to join the terminal tab strip");
        };
        assert_eq!(
            panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            [terminal, browser]
        );
        assert_eq!(*active, browser);
    }

    #[test]
    fn directional_drop_of_a_lone_tab_keeps_it_live_and_fills_the_vacated_half() {
        let registry = SessionRegistry::new().unwrap();
        let moved = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let moved_pid = registry.pane_process_id(moved).unwrap();

        registry
            .move_pane_to_split(moved, moved, DropPlacement::Bottom)
            .unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: top,
            second: bottom,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected the lone tab drop to create a filled split");
        };
        let replacement = first_pane_in_layout(top);
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_ne!(replacement, moved);
        assert_eq!(first_pane_in_layout(bottom), moved);
        assert_eq!(registry.pane_process_id(moved).unwrap(), moved_pid);
        assert!(registry.pane_process_id(replacement).unwrap().is_some());
        assert_eq!(registry.state().unwrap().1.len(), 2);
    }

    #[test]
    fn closing_the_last_terminal_leaves_a_saved_empty_workspace_until_explicit_reopen() {
        let registry = SessionRegistry::new().unwrap();
        let initial = registry.snapshot().unwrap();
        let workspace_id = initial.workspaces[0].id;
        let first = first_pane_id(&initial).unwrap();
        let second = registry.create_pane(first, SplitAxis::Vertical).unwrap();
        let second_pid = registry.pane_process_id(second).unwrap();

        registry.close_pane(first).unwrap();

        let snapshot = registry.snapshot().unwrap();
        assert_eq!(first_pane_id(&snapshot), Some(second));
        assert_eq!(registry.pane_process_id(second).unwrap(), second_pid);
        assert!(registry.pane_process_id(first).is_err());

        registry.close_pane(second).unwrap();

        let empty = registry.snapshot().unwrap();
        assert_eq!(empty.workspaces.len(), 1);
        assert_eq!(empty.workspaces[0].id, workspace_id);
        assert!(empty.workspaces[0].tabs.is_empty());
        assert_eq!(empty.workspaces[0].active_terminal_count, 0);
        assert!(registry.state().unwrap().1.is_empty());

        let reopened = registry.create_workspace_terminal(workspace_id).unwrap();
        let reopened_snapshot = registry.snapshot().unwrap();
        assert_eq!(first_pane_id(&reopened_snapshot), Some(reopened));
        assert_eq!(reopened_snapshot.workspaces[0].active_terminal_count, 1);
        assert!(registry.create_workspace_terminal(workspace_id).is_err());
    }

    #[test]
    fn natural_shell_exit_stays_visible_until_explicit_layout_close() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let exiting = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        registry.write_input(exiting, b"exit 7\r").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = registry.snapshot().unwrap();
            let pane = snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.tabs)
                .find_map(|tab| pane_in_layout(&tab.layout, exiting))
                .expect("exited pane must remain in its layout");
            if pane.shell.contains("exited") {
                assert!(pane.shell.contains('7'));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "natural child exit was not reflected in pane metadata"
            );
            thread::sleep(Duration::from_millis(25));
        }

        assert!(registry.pane(exiting).is_ok());
        registry.close_pane(exiting).unwrap();
        assert!(registry.pane(exiting).is_err());
        assert_eq!(first_pane_id(&registry.snapshot().unwrap()), Some(first));
    }

    #[test]
    fn pane_local_split_only_mutates_the_explicit_second_pane() {
        let registry = SessionRegistry::new().unwrap();
        let first = first_pane_id(&registry.snapshot().unwrap()).unwrap();
        let second = registry.create_pane(first, SplitAxis::Horizontal).unwrap();
        let nested = registry.create_pane(second, SplitAxis::Vertical).unwrap();

        let snapshot = registry.snapshot().unwrap();
        let PaneLayout::Split {
            axis,
            first: left,
            second: right,
            ..
        } = &snapshot.workspaces[0].tabs[0].layout
        else {
            panic!("expected outer two pane columns");
        };
        assert_eq!(*axis, SplitAxis::Horizontal);
        assert!(matches!(&**left, PaneLayout::Leaf { pane } if pane.id == first));
        let PaneLayout::Split {
            axis,
            first: top,
            second: bottom,
            ..
        } = &**right
        else {
            panic!("split control must split the targeted second pane");
        };
        assert_eq!(*axis, SplitAxis::Vertical);
        assert_eq!(first_pane_in_layout(top), second);
        assert_eq!(first_pane_in_layout(bottom), nested);
    }
}
