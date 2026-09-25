//! Pane lifecycle: creation, input, identity overrides, and close/reattach.
use super::{
    InitialTerminalSpawn, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    TerminalRuntimePane, encode_desired_state,
};
use crate::gallery::import_gallery_image;
use crate::layout::{
    add_tab, detach_pane, find_pane_in_snapshot, find_pane_mut_in_snapshot, layout_contains,
    split_layout, workspace_id_for_pane,
};
use crate::persistence;
use crate::persistence::{MAX_TABS_PER_WORKSPACE, MAX_TITLE_CHARS, validate_title};
use crate::process::{fallback_cwd, local_spawn_dir, shell_title};
use crate::pty::PtySession;
use crate::registry::bots::{bot_for_pane, bot_spawn_dir, prune_bot_threads};
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
use std::path::{Path, PathBuf};
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

fn first_gallery_pane(layout: &PaneLayout) -> Option<Uuid> {
    match layout {
        PaneLayout::Leaf { pane } => pane.kind.is_gallery().then_some(pane.id),
        PaneLayout::Stack { panes, .. } => panes
            .iter()
            .find(|pane| pane.kind.is_gallery())
            .map(|pane| pane.id),
        PaneLayout::Split { first, second, .. } => {
            first_gallery_pane(first).or_else(|| first_gallery_pane(second))
        }
    }
}

impl SessionRegistry {
    pub fn create_pane(&self, target_pane: Uuid, axis: SplitAxis) -> Result<Uuid> {
        {
            let state = self.state.read();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            state.require_terminal_layout_pane(target_pane)?;
            state.refuse_bot_pane(target_pane)?;
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
            state.refuse_bot_pane(target_pane)?;
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
                    .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone(), true))
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
        state.refuse_bot_pane(target_pane)?;
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
            status_changed_at_ms: 0,
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        if !add_tab(&mut tab.layout, target_pane, pane, true) {
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

    pub fn create_group_gallery(&self, target_pane: Uuid, activate: bool) -> Result<Uuid> {
        let pane_id = Uuid::new_v4();
        let mut state = self.state.write();
        state.refuse_bot_pane(target_pane)?;
        if state.panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let tab = state
            .snapshot
            .workspaces
            .iter_mut()
            .flat_map(|workspace| &mut workspace.tabs)
            .find(|tab| layout_contains(&tab.layout, target_pane))
            .with_context(|| format!("target pane {target_pane} does not exist"))?;
        let pane = Pane {
            id: pane_id,
            kind: PaneKind::Gallery,
            title: "Gallery".to_owned(),
            shell: String::new(),
            color: None,
            identity: TerminalIdentity::default(),
            status: PaneStatus::default(),
            status_changed_at_ms: 0,
            custom_title: None,
            profile_override: None,
            custom_icon: None,
        };
        if !add_tab(&mut tab.layout, target_pane, pane, activate) {
            bail!("target pane {target_pane} does not exist");
        }
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Gallery,
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
                session: self.spawn_local_transport(pane_id, workspace_id, None, cwd)?,
                kind: RuntimePaneKind::Local,
                pane_title: "Terminal 1".to_owned(),
                pane_shell: shell_title(),
                tab_title: "Terminals".to_owned(),
            }),
            WorkspaceConnection::SystemSsh { destination, .. } => Ok(InitialTerminalSpawn {
                session: PtySession::spawn_ssh(pane_id, workspace_id, destination, working_dir)?,
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
        self.ensure_workspace_accepts_workstation_tabs(workspace_id)?;
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
                status_changed_at_ms: 0,
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            workspace.tabs.push(Tab {
                owner_thread: None,
                id: Uuid::new_v4(),
                title: tab_title,
                custom_title: None,
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                owner_bot: None,
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
            state.refuse_bot_pane(target_pane)?;
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
        let session = PtySession::spawn_ssh(pane_id, workspace_id, host, None)?;
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
                status_changed_at_ms: 0,
                custom_title: None,
                profile_override: None,
                custom_icon: None,
            };
            let did_add = state.snapshot.workspaces.iter_mut().any(|workspace| {
                workspace
                    .tabs
                    .iter_mut()
                    .any(|tab| add_tab(&mut tab.layout, target_pane, pane.clone(), true))
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
        self.ensure_workspace_accepts_workstation_tabs(workspace_id)?;
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
            owner_thread: None,
            id: Uuid::new_v4(),
            title: title.clone(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            owner_bot: None,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: pane_id,
                    kind: PaneKind::Browser { url },
                    title,
                    shell: String::new(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    status: hh_protocol::PaneStatus::default(),
                    status_changed_at_ms: 0,
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

    pub fn create_gallery_tab(&self, workspace_id: Uuid) -> Result<Uuid> {
        self.ensure_workspace_accepts_workstation_tabs(workspace_id)?;
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
            owner_thread: None,
            id: Uuid::new_v4(),
            title: "Gallery".to_owned(),
            custom_title: None,
            project_dir: None,
            color: None,
            custom_icon: None,
            parent_tab: None,
            pinned: false,
            owner_bot: None,
            layout: PaneLayout::Leaf {
                pane: Pane {
                    id: pane_id,
                    kind: PaneKind::Gallery,
                    title: "Gallery".to_owned(),
                    shell: String::new(),
                    color: None,
                    identity: TerminalIdentity::default(),
                    status: PaneStatus::default(),
                    status_changed_at_ms: 0,
                    custom_title: None,
                    profile_override: None,
                    custom_icon: None,
                },
            },
        });
        state.panes.insert(
            pane_id,
            RuntimePane {
                backend: RuntimePaneBackend::Gallery,
            },
        );
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        self.write_snapshot(&bytes)?;
        Ok(pane_id)
    }

    pub fn add_gallery_image(
        &self,
        workspace_id: Uuid,
        origin_pane: Option<Uuid>,
        source: &str,
    ) -> Result<(PathBuf, Uuid)> {
        let path = import_gallery_image(workspace_id, Path::new(source))?;
        let (gallery_pane, group_origin) = {
            let state = self.state.read();
            let workspace = state
                .snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            let origin_tab = origin_pane.and_then(|origin| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| layout_contains(&tab.layout, origin))
            });
            let gallery_pane = origin_tab
                .and_then(|tab| first_gallery_pane(&tab.layout))
                .or_else(|| {
                    workspace
                        .tabs
                        .iter()
                        .find_map(|tab| first_gallery_pane(&tab.layout))
                });
            (gallery_pane, origin_pane.filter(|_| origin_tab.is_some()))
        };
        let pane_id = match (gallery_pane, group_origin) {
            (Some(pane_id), _) => pane_id,
            (None, Some(origin_pane)) => self.create_group_gallery(origin_pane, false)?,
            (None, None) => self.create_gallery_tab(workspace_id)?,
        };
        Ok((path, pane_id))
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
        let terminal = self
            .state
            .read()
            .panes
            .get(&pane_id)
            .and_then(RuntimePane::terminal)
            .map(|terminal| Arc::clone(&terminal.session));
        if let Some(terminal) = terminal {
            terminal.rename_tmux_window(title)?;
        }
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
        let previous_snapshot = state.snapshot.clone();
        let pane = find_pane_mut_in_snapshot(&mut state.snapshot, pane_id)
            .with_context(|| format!("pane {pane_id} does not exist"))?;
        if pane.custom_title.is_none() {
            pane.custom_title = Some(pane.title.clone());
        }
        pane.custom_icon = icon;
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        if let Err(error) = self.write_snapshot(&bytes) {
            state.snapshot = previous_snapshot;
            return Err(error);
        }
        Ok(())
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
                let removed_tab = workspace.tabs.remove(tab_index);
                for tab in &mut workspace.tabs {
                    if tab.parent_tab == Some(removed_tab.id) {
                        tab.parent_tab = None;
                    }
                }
            }
            prune_bot_threads(workspace);
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
        let (kind, cwd, workspace_id, managed_tmux, bot_id) = {
            let state = self.state.read();
            let runtime = state.terminal_pane(pane_id)?;
            if runtime.exit_status.is_none() {
                bail!("this terminal is still live");
            }
            let workspace_id = workspace_id_for_pane(&state.snapshot, pane_id)
                .with_context(|| format!("pane {pane_id} has no workstation"))?;
            let bot_id = bot_for_pane(&state.snapshot, pane_id);
            // A bot's fresh shell always starts in its home.
            let cwd = bot_id
                .and_then(|bot| {
                    bot_spawn_dir(&state.snapshot, self.bots_dir().ok().as_deref(), bot)
                })
                .unwrap_or_else(|| runtime.last_valid_cwd.clone());
            (
                runtime.kind.clone(),
                cwd,
                workspace_id,
                runtime.session.tmux_ids().is_some(),
                bot_id,
            )
        };
        let session = match &kind {
            RuntimePaneKind::Local if managed_tmux => PtySession::spawn_tmux(
                pane_id,
                workspace_id,
                bot_id,
                &cwd,
                &self.client_for_workspace(workspace_id)?,
            )?,
            RuntimePaneKind::Local => PtySession::spawn_local(pane_id, workspace_id, bot_id, &cwd)?,
            RuntimePaneKind::SystemSsh { host } => {
                PtySession::spawn_ssh(pane_id, workspace_id, host, None)?
            }
            RuntimePaneKind::TmuxLocal { session_id } => {
                PtySession::spawn_tmux_local(pane_id, session_id)?
            }
            RuntimePaneKind::TmuxSystemSsh { host, session_id } => {
                PtySession::spawn_tmux_ssh(pane_id, host, session_id)?
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
        let _ = previous.terminate_and_wait();
        drop(previous);
        if let Some(bot_id) = bot_id {
            self.relaunch_recovered_bot(bot_id, pane_id);
        }
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

    /// Offers a desktop-materialized PNG to the pane's application as a
    /// kitty paste event. The pane must have enhanced paste enabled; the
    /// image file is left untouched otherwise, so the desktop can fall back
    /// to typing its path.
    pub fn paste_image(&self, pane_id: Uuid, image_path: &str, text: Option<String>) -> Result<()> {
        let pane = self.pane(pane_id)?;
        if !pane.enhanced_paste() {
            bail!("pane {pane_id} has not enabled enhanced paste");
        }
        if text
            .as_ref()
            .is_some_and(|text| text.len() > crate::paste_events::MAX_PASTE_TEXT_BYTES)
        {
            bail!("pasted text exceeds the 1 MiB limit");
        }
        let png = crate::paste_events::take_paste_image(
            Path::new(image_path),
            &crate::paste_events::paste_directory(),
        )?;
        pane.paste_image(png, text)
    }

    pub fn write_input(&self, pane_id: Uuid, bytes: &[u8]) -> Result<()> {
        self.write_input_with_delivery(pane_id, bytes)
            .map_err(anyhow::Error::new)
    }

    pub(crate) fn write_input_with_delivery(
        &self,
        pane_id: Uuid,
        bytes: &[u8],
    ) -> std::result::Result<(), crate::pty::InputDeliveryError> {
        let pane = self.pane(pane_id).map_err(|error| {
            crate::pty::InputDeliveryError::definitely_unsent(format!("{error:#}"))
        })?;
        pane.write_input(bytes)?;
        let needs_status_change = {
            let state = self.state.read();
            find_pane_in_snapshot(&state.snapshot, pane_id).is_some_and(|pane| {
                matches!(
                    pane.status,
                    PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention
                )
            })
        };
        if needs_status_change {
            self.state
                .write()
                .set_pane_status(pane_id, PaneStatus::Working);
        }
        Ok(())
    }

    pub fn authorized_write_input(
        &self,
        authority: &hh_protocol::PaneAuthority,
        bytes: &[u8],
    ) -> Result<()> {
        self.authorized_write_input_with_delivery(authority, bytes)
            .map_err(anyhow::Error::new)
    }

    pub(crate) fn authorized_write_input_with_delivery(
        &self,
        authority: &hh_protocol::PaneAuthority,
        bytes: &[u8],
    ) -> std::result::Result<(), crate::pty::InputDeliveryError> {
        let needs_status_change = {
            // Keep authority stable through delivery without excluding other readers.
            let state = self.state.read();
            state
                .authorized_terminal(authority)
                .map_err(|error| {
                    crate::pty::InputDeliveryError::definitely_unsent(format!("{error:#}"))
                })?
                .session
                .write_input(bytes)?;
            find_pane_in_snapshot(&state.snapshot, authority.pane_id).is_some_and(|pane| {
                matches!(
                    pane.status,
                    PaneStatus::NeedsApproval | PaneStatus::NeedsInput | PaneStatus::Attention
                )
            })
        };
        if needs_status_change {
            let mut state = self.state.write();
            if state.authorized_terminal(authority).is_ok() {
                state.set_pane_status(authority.pane_id, PaneStatus::Working);
            }
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
#[path = "panes_tests.rs"]
mod tests;
