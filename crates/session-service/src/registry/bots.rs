//! Bots: the reserved Bots workspace, bot terminals, and bot-owned workers.
use super::{
    RegistryState, RuntimePane, RuntimePaneBackend, RuntimePaneKind, SessionRegistry,
    TerminalRuntimePane, encode_desired_state,
};
use crate::bots::{
    BotLaunch, PreparedLaunch, bot_home, bots_directory, prepare_launch, remove_bot_files,
};
use crate::layout::{find_pane_in_snapshot, layout_contains};
use crate::persistence::{
    MAX_INSTRUCTIONS_CHARS, MAX_TABS_PER_WORKSPACE, MAX_WORKSPACES, validate_title,
};
use crate::process::{fallback_cwd, hh_cli_path, local_spawn_dir, shell_title, valid_local_cwd};
use crate::pty::{MAX_INPUT_FRAME, PtySession};
use crate::registry::identity::{refresh_workspace_activity, set_pane_runtime_label};
use crate::registry::workspaces::next_workspace_order;
use anyhow::{Context, Result, bail};
use hh_protocol::{
    BotSettings, BotSpec, MAX_PANES, NotificationKind, PaneLayout, PaneStatus, SessionSnapshot,
    Tab, TerminalProfile, Workspace, WorkspaceConnection, WorkspaceKind, validate_workspace_dir,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const BOTS_WORKSPACE_TITLE: &str = "Bots";
/// Longest wait for a fresh shell's first output before its command is typed.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(3);
const SHELL_READY_POLL: Duration = Duration::from_millis(20);

/// The bot tab containing `pane_id`, when that pane is a bot terminal.
pub(crate) fn bot_tab_for_pane(snapshot: &SessionSnapshot, pane_id: Uuid) -> Option<Uuid> {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| workspace.is_bots())
        .flat_map(|workspace| &workspace.tabs)
        .find(|tab| tab.bot.is_some() && layout_contains(&tab.layout, pane_id))
        .map(|tab| tab.id)
}

/// Clears every reference to deleted bots and removes their launch files and
/// default home folders from `bots_dir`.
pub(crate) fn forget_bots(
    snapshot: &mut SessionSnapshot,
    bots: &HashSet<Uuid>,
    bots_dir: Option<&Path>,
) {
    if bots.is_empty() {
        return;
    }
    for workspace in &mut snapshot.workspaces {
        if workspace.owner_bot.is_some_and(|bot| bots.contains(&bot)) {
            workspace.owner_bot = None;
        }
        for tab in &mut workspace.tabs {
            if tab.owner_bot.is_some_and(|bot| bots.contains(&bot)) {
                tab.owner_bot = None;
            }
        }
    }
    if let Some(directory) = bots_dir {
        for bot in bots {
            remove_bot_files(directory, *bot);
        }
    }
}

/// The folder a fresh shell of bot `tab_id` starts in, when it can be
/// prepared; failures are logged and the caller keeps its own directory.
pub(crate) fn bot_spawn_dir(
    snapshot: &SessionSnapshot,
    bots_dir: Option<&Path>,
    tab_id: Uuid,
) -> Option<PathBuf> {
    let spec = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.tabs)
        .find(|tab| tab.id == tab_id)?
        .bot
        .as_ref()?;
    match bot_home(bots_dir?, tab_id, spec) {
        Ok(home) => Some(home),
        Err(error) => {
            eprintln!("could not prepare the home of bot {tab_id}: {error:#}");
            None
        }
    }
}

/// Number of regular workstations; the Bots workspace never counts.
pub(crate) fn workstation_count(snapshot: &SessionSnapshot) -> usize {
    snapshot
        .workspaces
        .iter()
        .filter(|workspace| !workspace.is_bots())
        .count()
}

fn normalize_bot_name(name: Option<&str>) -> Result<Option<String>> {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    validate_title(name, "bot")?;
    Ok(Some(name.to_owned()))
}

fn normalize_bot_instructions(instructions: Option<String>) -> Result<Option<String>> {
    let instructions = instructions
        .map(|instructions| instructions.trim().to_owned())
        .filter(|instructions| !instructions.is_empty());
    if instructions
        .as_deref()
        .is_some_and(|instructions| instructions.chars().count() > MAX_INSTRUCTIONS_CHARS)
    {
        bail!("bot instructions exceed {MAX_INSTRUCTIONS_CHARS} characters");
    }
    Ok(instructions)
}

/// Types `line` and Enter into a freshly spawned shell once it has drawn its
/// first output, so the command is not echoed ahead of the prompt.
fn type_when_ready(session: Arc<PtySession>, line: String) {
    let spawned = thread::Builder::new()
        .name("hh-type-command".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + SHELL_READY_TIMEOUT;
            while session.current_revision() == 0 && Instant::now() < deadline {
                thread::sleep(SHELL_READY_POLL);
            }
            let mut bytes = line.into_bytes();
            bytes.push(b'\r');
            if let Err(error) = session.write_input(&bytes) {
                eprintln!("could not type a command into a new terminal: {error}");
            }
        });
    if let Err(error) = spawned {
        eprintln!("could not start the command typing thread: {error}");
    }
}

/// A bot tab's identity and launch location, read under the registry lock.
struct BotTarget {
    workspace_id: Uuid,
    pane_id: Uuid,
    name: String,
    project_dir: Option<String>,
    spec: BotSpec,
}

impl RegistryState {
    fn bot_target(&self, tab_id: Uuid) -> Result<BotTarget> {
        let (workspace, tab) = self
            .snapshot
            .workspaces
            .iter()
            .filter(|workspace| workspace.is_bots())
            .find_map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| tab.id == tab_id)
                    .map(|tab| (workspace, tab))
            })
            .with_context(|| format!("bot {tab_id} does not exist"))?;
        let spec = tab
            .bot
            .clone()
            .with_context(|| format!("bot {tab_id} does not exist"))?;
        let PaneLayout::Leaf { pane } = &tab.layout else {
            bail!("bot {tab_id} does not hold a single terminal");
        };
        Ok(BotTarget {
            workspace_id: workspace.id,
            pane_id: pane.id,
            name: tab
                .custom_title
                .clone()
                .unwrap_or_else(|| tab.title.clone()),
            project_dir: tab.project_dir.clone(),
            spec,
        })
    }
}

impl SessionRegistry {
    pub fn set_bot_settings(&self, settings: BotSettings) -> Result<()> {
        let mut state = self.state.write();
        let previous = std::mem::replace(&mut state.snapshot.bots, settings);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let written = encode_desired_state(&state).and_then(|bytes| self.write_snapshot(&bytes));
        if written.is_err() {
            state.snapshot.bots = previous;
            state.snapshot.revision = state.snapshot.revision.saturating_sub(1);
        }
        written
    }

    /// Creates a bot tab in the Bots workspace and types its agent's launch
    /// command into the new shell. Returns `(workspace_id, tab_id, pane_id)`.
    pub fn create_bot(
        &self,
        name: Option<&str>,
        agent: TerminalProfile,
        working_dir: Option<String>,
        instructions: Option<String>,
    ) -> Result<(Uuid, Uuid, Uuid)> {
        let title = normalize_bot_name(name)?.unwrap_or_else(|| agent.display_name().to_owned());
        if let Some(dir) = working_dir.as_deref() {
            validate_workspace_dir(dir).map_err(anyhow::Error::from)?;
        }
        let spec = BotSpec {
            agent,
            instructions: normalize_bot_instructions(instructions)?,
            home: None,
        };
        let tab_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let launch = self.prepare_bot_launch(tab_id, &title, working_dir.as_deref(), &spec)?;
        if self.state.read().panes.len() >= MAX_PANES {
            bail!("pane limit of {MAX_PANES} reached");
        }
        let workspace_id = self.ensure_bots_workspace()?;
        let cwd = launch.home.clone();
        let session = self.spawn_local_transport(pane_id, workspace_id, Some(tab_id), &cwd)?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let previous = state.snapshot.clone();
            let mut pane = state.new_pane(pane_id, Some(cwd.as_path()));
            pane.title.clone_from(&title);
            pane.custom_title = Some(title.clone());
            pane.profile_override = Some(agent);
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .context("the Bots workspace disappeared")?;
            if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("bot limit of {MAX_TABS_PER_WORKSPACE} reached");
            }
            workspace.tabs.push(Tab {
                id: tab_id,
                title,
                custom_title: None,
                project_dir: working_dir,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                bot: Some(spec),
                owner_bot: None,
                layout: PaneLayout::Leaf { pane },
            });
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                local_terminal_runtime(Arc::clone(&session), cwd.clone()),
            );
            self.commit_or_restore(&mut state, previous, &[pane_id])
        })();
        if let Err(error) = result {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        self.start_bot_agent(tab_id, session, launch);
        Ok((workspace_id, tab_id, pane_id))
    }

    /// Switches a bot to another agent and relaunches its terminal.
    pub fn set_bot_agent(&self, tab_id: Uuid, agent: TerminalProfile) -> Result<()> {
        let BotTarget {
            name,
            project_dir,
            mut spec,
            ..
        } = self.state.read().bot_target(tab_id)?;
        spec.agent = agent;
        let launch = self.prepare_bot_launch(tab_id, &name, project_dir.as_deref(), &spec)?;
        {
            let mut state = self.state.write();
            let previous = state.snapshot.clone();
            let tab = state
                .snapshot
                .workspaces
                .iter_mut()
                .flat_map(|workspace| &mut workspace.tabs)
                .find(|tab| tab.id == tab_id && tab.bot.is_some())
                .with_context(|| format!("bot {tab_id} does not exist"))?;
            tab.bot = Some(spec);
            let PaneLayout::Leaf { pane } = &mut tab.layout else {
                bail!("bot {tab_id} does not hold a single terminal");
            };
            pane.profile_override = Some(agent);
            self.commit_or_restore(&mut state, previous, &[])?;
        }
        self.relaunch_bot(tab_id, launch)
    }

    /// Terminates a bot's terminal and launches its agent again.
    pub fn restart_bot(&self, tab_id: Uuid) -> Result<()> {
        let target = self.state.read().bot_target(tab_id)?;
        let launch = self.prepare_bot_launch(
            tab_id,
            &target.name,
            target.project_dir.as_deref(),
            &target.spec,
        )?;
        self.relaunch_bot(tab_id, launch)
    }

    /// Moves a bot to a custom home folder, or back to its default home with
    /// `None`, and relaunches its terminal there.
    pub fn set_bot_home(&self, tab_id: Uuid, home: Option<String>) -> Result<()> {
        if let Some(home) = home.as_deref() {
            validate_workspace_dir(home).map_err(anyhow::Error::from)?;
            if !valid_local_cwd(Path::new(home)) {
                bail!("bot home {home} is not an existing directory");
            }
        }
        let BotTarget {
            name,
            project_dir,
            mut spec,
            ..
        } = self.state.read().bot_target(tab_id)?;
        spec.home = home;
        let launch = self.prepare_bot_launch(tab_id, &name, project_dir.as_deref(), &spec)?;
        {
            let mut state = self.state.write();
            let previous = state.snapshot.clone();
            let tab = state
                .snapshot
                .workspaces
                .iter_mut()
                .flat_map(|workspace| &mut workspace.tabs)
                .find(|tab| tab.id == tab_id && tab.bot.is_some())
                .with_context(|| format!("bot {tab_id} does not exist"))?;
            tab.bot = Some(spec);
            self.commit_or_restore(&mut state, previous, &[])?;
        }
        self.relaunch_bot(tab_id, launch)
    }

    /// Opens a worker terminal tab and types `command` into its shell.
    /// Returns `(workspace_id, tab_id, pane_id)`.
    pub fn create_worker(
        &self,
        workspace_id: Option<Uuid>,
        working_dir: Option<&str>,
        title: Option<&str>,
        command: Option<&str>,
        requester_pane: Option<Uuid>,
    ) -> Result<(Uuid, Uuid, Uuid)> {
        let title = title.map(str::trim).filter(|title| !title.is_empty());
        if let Some(title) = title {
            validate_title(title, "worker")?;
        }
        if let Some(command) = command {
            if command.chars().any(char::is_control) {
                bail!("worker command must be a single line without control characters");
            }
            if command.len() >= MAX_INPUT_FRAME {
                bail!("worker command exceeds {MAX_INPUT_FRAME} bytes");
            }
        }
        if let Some(dir) = working_dir {
            validate_workspace_dir(dir).map_err(anyhow::Error::from)?;
        }
        let owner_bot = match requester_pane {
            Some(pane_id) => {
                let state = self.state.read();
                if find_pane_in_snapshot(&state.snapshot, pane_id).is_none() {
                    bail!("requester pane {pane_id} does not exist");
                }
                bot_tab_for_pane(&state.snapshot, pane_id)
            }
            None => None,
        };
        let workspace_id = match (workspace_id, owner_bot) {
            (Some(workspace_id), _) => {
                self.ensure_workspace_accepts_workstation_tabs(workspace_id)?;
                workspace_id
            }
            (None, Some(bot)) => self.bot_workstation(bot)?,
            (None, None) => {
                bail!("a worker needs a workspace_id unless it is requested from a bot terminal")
            }
        };
        let (connection, workspace_dir) = {
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
            (workspace.connection.clone(), workspace.working_dir.clone())
        };
        let (cwd, remote_dir) = match connection {
            WorkspaceConnection::Local => match working_dir {
                Some(dir) if valid_local_cwd(Path::new(dir)) => (PathBuf::from(dir), None),
                Some(dir) => bail!("worker directory {dir} is not an existing directory"),
                None => (local_spawn_dir(workspace_dir.as_deref())?, None),
            },
            WorkspaceConnection::SystemSsh { .. } => (
                fallback_cwd()?,
                working_dir.map(str::to_owned).or(workspace_dir),
            ),
        };
        let pane_id = Uuid::new_v4();
        let tab_id = Uuid::new_v4();
        let (session, kind) =
            self.spawn_pane_for_workspace(pane_id, workspace_id, &cwd, remote_dir.as_deref())?;
        let result = (|| {
            let mut state = self.state.write();
            if state.panes.len() >= MAX_PANES {
                bail!("pane limit of {MAX_PANES} reached");
            }
            let previous = state.snapshot.clone();
            let mut pane = state.new_pane(pane_id, Some(cwd.as_path()));
            if matches!(kind, RuntimePaneKind::SystemSsh { .. }) {
                "ssh".clone_into(&mut pane.shell);
            }
            if let Some(title) = title {
                title.clone_into(&mut pane.title);
                pane.custom_title = Some(title.to_owned());
            }
            let workspace = state
                .snapshot
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == workspace_id)
                .with_context(|| format!("workstation {workspace_id} does not exist"))?;
            if workspace.tabs.len() >= MAX_TABS_PER_WORKSPACE {
                bail!("tab limit of {MAX_TABS_PER_WORKSPACE} reached");
            }
            workspace.tabs.push(Tab {
                id: tab_id,
                title: pane.title.clone(),
                custom_title: title.map(str::to_owned),
                project_dir: None,
                color: None,
                custom_icon: None,
                parent_tab: None,
                pinned: false,
                bot: None,
                owner_bot,
                layout: PaneLayout::Leaf { pane },
            });
            workspace.active_terminal_count = workspace.active_terminal_count.saturating_add(1);
            state.panes.insert(
                pane_id,
                RuntimePane {
                    backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
                        session: Arc::clone(&session),
                        last_valid_cwd: cwd.clone(),
                        kind,
                        recovered: false,
                        exit_status: None,
                        detected_command_profile: None,
                        omp_title_status: None,
                    }),
                },
            );
            self.commit_or_restore(&mut state, previous, &[pane_id])
        })();
        if let Err(error) = result {
            let _ = session.terminate_and_wait();
            return Err(error);
        }
        if let Some(command) = command {
            type_when_ready(session, command.to_owned());
        }
        Ok((workspace_id, tab_id, pane_id))
    }

    /// Re-types a recovered bot's launch command into its fresh shell. Agent
    /// discovery can take seconds, so this runs off the recovery path.
    pub(crate) fn relaunch_recovered_bot(&self, tab_id: Uuid) {
        let registry = self.clone();
        let spawned = thread::Builder::new()
            .name("hh-bot-relaunch".to_owned())
            .spawn(move || {
                let launched = (|| {
                    let target = registry.state.read().bot_target(tab_id)?;
                    let launch = registry.prepare_bot_launch(
                        tab_id,
                        &target.name,
                        target.project_dir.as_deref(),
                        &target.spec,
                    )?;
                    Ok::<_, anyhow::Error>((registry.pane(target.pane_id)?, launch))
                })();
                match launched {
                    Ok((session, launch)) => registry.start_bot_agent(tab_id, session, launch),
                    Err(error) => registry
                        .notify_bot(tab_id, &format!("could not start its agent: {error:#}")),
                }
            });
        if let Err(error) = spawned {
            self.notify_bot(tab_id, &format!("could not start its agent: {error:#}"));
        }
    }

    /// Posts a message notification on the bot's pane, prefixed with its name.
    fn notify_bot(&self, tab_id: Uuid, message: &str) {
        let mut state = self.state.write();
        if let Ok(target) = state.bot_target(tab_id) {
            state.append_notification(
                target.pane_id,
                NotificationKind::Message,
                Some(format!("{} {message}", target.name)),
                crate::now_ms(),
            );
        }
    }

    /// Types the agent's launch command into the bot's fresh shell and tells
    /// the user when a custom home kept its own `AGENTS.md`.
    fn start_bot_agent(&self, tab_id: Uuid, session: Arc<PtySession>, launch: PreparedLaunch) {
        type_when_ready(session, launch.command);
        if !launch.context_written {
            self.notify_bot(
                tab_id,
                &format!(
                    "did not get its instructions: {} already has its own AGENTS.md.",
                    launch.home.display()
                ),
            );
        }
    }

    /// The service's `<state>/bots` directory; bots need a persistent registry.
    pub(crate) fn bots_dir(&self) -> Result<PathBuf> {
        let state_dir = self
            .store
            .as_ref()
            .and_then(|store| store.directory())
            .context("bots need a persistent session state directory")?;
        Ok(bots_directory(state_dir))
    }

    fn prepare_bot_launch(
        &self,
        tab_id: Uuid,
        name: &str,
        project_dir: Option<&str>,
        spec: &BotSpec,
    ) -> Result<PreparedLaunch> {
        let mut agents = self.coding_agents(false)?;
        if !agents.iter().any(|agent| agent.profile == spec.agent) {
            agents = self.coding_agents(true)?;
        }
        let bot = BotLaunch {
            tab_id,
            name,
            project_dir,
            spec,
        };
        prepare_launch(&bot, &agents, &self.bots_dir()?, hh_cli_path().as_deref())
    }

    /// Terminates the bot's terminal, respawns the same pane in a fresh shell
    /// in the bot's home and types the launch command into it.
    fn relaunch_bot(&self, tab_id: Uuid, launch: PreparedLaunch) -> Result<()> {
        let (target, previous) = {
            let state = self.state.read();
            let target = state.bot_target(tab_id)?;
            let previous = state
                .panes
                .get(&target.pane_id)
                .and_then(RuntimePane::terminal)
                .map(|terminal| Arc::clone(&terminal.session));
            (target, previous)
        };
        if let Some(previous) = previous {
            previous
                .terminate_and_wait()
                .context("terminate the bot terminal")?;
        }
        let session = self.spawn_local_transport(
            target.pane_id,
            target.workspace_id,
            Some(tab_id),
            &launch.home,
        )?;
        let mut state = self.state.write();
        if state.bot_target(tab_id)?.pane_id != target.pane_id {
            drop(state);
            let _ = session.terminate_and_wait();
            bail!("bot {tab_id} changed while restarting");
        }
        let replaced = state.panes.insert(
            target.pane_id,
            local_terminal_runtime(Arc::clone(&session), launch.home.clone()),
        );
        set_pane_runtime_label(
            &mut state.snapshot,
            target.pane_id,
            false,
            None,
            &shell_title(),
        );
        state.set_pane_status(target.pane_id, PaneStatus::Idle);
        refresh_workspace_activity(&mut state);
        state.snapshot.revision = state.snapshot.revision.saturating_add(1);
        let bytes = encode_desired_state(&state)?;
        drop(state);
        drop(replaced);
        self.start_bot_agent(tab_id, session, launch);
        self.write_snapshot(&bytes)
    }

    /// The Bots workspace, created and persisted on first use.
    fn ensure_bots_workspace(&self) -> Result<Uuid> {
        let mut state = self.state.write();
        if let Some(workspace) = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.is_bots())
        {
            return Ok(workspace.id);
        }
        let previous = state.snapshot.clone();
        let workspace_id = Uuid::new_v4();
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        state.snapshot.workspaces.push(empty_local_workspace(
            workspace_id,
            BOTS_WORKSPACE_TITLE.to_owned(),
            order,
            WorkspaceKind::Bots,
        ));
        self.commit_or_restore(&mut state, previous, &[])?;
        Ok(workspace_id)
    }

    /// The workstation a bot's workers default to: the one it owns, or a new
    /// one titled after the bot in the bot's project folder (the user's home
    /// without one).
    fn bot_workstation(&self, bot: Uuid) -> Result<Uuid> {
        let mut state = self.state.write();
        if let Some(workspace) = state
            .snapshot
            .workspaces
            .iter()
            .find(|workspace| !workspace.is_bots() && workspace.owner_bot == Some(bot))
        {
            return Ok(workspace.id);
        }
        let (name, working_dir) = state
            .snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .find(|tab| tab.id == bot)
            .map(|tab| {
                (
                    tab.custom_title
                        .clone()
                        .unwrap_or_else(|| tab.title.clone()),
                    tab.project_dir.clone(),
                )
            })
            .with_context(|| format!("bot {bot} does not exist"))?;
        if workstation_count(&state.snapshot) >= MAX_WORKSPACES {
            bail!("workstation limit of {MAX_WORKSPACES} reached");
        }
        let previous = state.snapshot.clone();
        let workspace_id = Uuid::new_v4();
        let order = next_workspace_order(&state.snapshot.workspaces, false);
        let mut workspace =
            empty_local_workspace(workspace_id, name, order, WorkspaceKind::Workstation);
        workspace.working_dir = working_dir;
        workspace.owner_bot = Some(bot);
        state.snapshot.workspaces.push(workspace);
        self.commit_or_restore(&mut state, previous, &[])?;
        Ok(workspace_id)
    }

    /// Persists the mutated desired state, or restores `previous` and drops
    /// the runtimes of `new_panes` when persistence fails.
    fn commit_or_restore(
        &self,
        state: &mut RegistryState,
        previous: SessionSnapshot,
        new_panes: &[Uuid],
    ) -> Result<()> {
        state.snapshot.revision = previous.revision.saturating_add(1);
        let written = encode_desired_state(state).and_then(|bytes| self.write_snapshot(&bytes));
        if written.is_err() {
            state.snapshot = previous;
            for pane_id in new_panes {
                state.panes.remove(pane_id);
            }
        }
        written
    }
}

fn local_terminal_runtime(session: Arc<PtySession>, cwd: PathBuf) -> RuntimePane {
    RuntimePane {
        backend: RuntimePaneBackend::Terminal(TerminalRuntimePane {
            session,
            last_valid_cwd: cwd,
            kind: RuntimePaneKind::Local,
            recovered: false,
            exit_status: None,
            detected_command_profile: None,
            omp_title_status: None,
        }),
    }
}

fn empty_local_workspace(id: Uuid, title: String, order: u32, kind: WorkspaceKind) -> Workspace {
    Workspace {
        id,
        title,
        color: None,
        pinned: false,
        pin_order: 0,
        order,
        active_terminal_count: 0,
        connection: WorkspaceConnection::Local,
        working_dir: None,
        kind,
        instructions: None,
        owner_bot: None,
        custom_icon: None,
        tabs: Vec::new(),
    }
}

#[cfg(test)]
#[path = "bots_tests.rs"]
mod tests;
