//! Bots: coding-agent CLIs running in the reserved Bots workspace. Bots never
//! appear among workstations; the sidebar's Bots mode lists and opens them.
use std::collections::HashMap;

use gpui::{Context, Pixels, Point};
use hh_protocol::{
    BotSettings, ClientRequest, CodingAgent, Pane, PaneStatus, ServiceResponse, SessionSnapshot,
    Tab, TerminalProfile, Workspace,
};
use uuid::Uuid;

use crate::helpers::{collect_terminal_tabs, find_pane, visible_panes, workspace_is_selectable};
use crate::notifications::{ActivitySection, activity_section};
use crate::view_models::{BotMenu, GroupRenameEditor, Modal, SidebarMode, WorkspaceCreationDialog};
use crate::{HhApp, max_pane_status};

mod settings;
mod view;

/// Installed coding agents reported by the session service.
#[derive(Debug, Default)]
pub(crate) struct CodingAgentsState {
    pub loading: bool,
    pub loaded: bool,
    pub agents: Vec<CodingAgent>,
    pub error: Option<String>,
}

/// The agent a new bot starts with: the configured default when installed,
/// else omp when installed, else the first installed agent.
pub(crate) fn default_bot_agent(
    configured: Option<TerminalProfile>,
    installed: &[CodingAgent],
) -> Option<TerminalProfile> {
    let is_installed =
        |profile: TerminalProfile| installed.iter().any(|agent| agent.profile == profile);
    configured
        .filter(|profile| is_installed(*profile))
        .or_else(|| is_installed(TerminalProfile::Omp).then_some(TerminalProfile::Omp))
        .or_else(|| installed.first().map(|agent| agent.profile))
}

/// The single terminal pane of a bot tab.
pub(crate) fn bot_pane(tab: &Tab) -> Option<&Pane> {
    visible_panes(&tab.layout)
        .first()
        .and_then(|pane_id| find_pane(&tab.layout, *pane_id))
}

/// A bot's display name: its rename, else the service-chosen title.
pub(crate) fn bot_name(tab: &Tab) -> &str {
    tab.custom_title.as_deref().unwrap_or(&tab.title)
}

/// A worker tab a bot opened, listed under that bot in the Bots sidebar.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BotWorker<'a> {
    pub(crate) workspace: &'a Workspace,
    pub(crate) tab: &'a Tab,
    /// The tab's visible pane, focused when the worker is opened.
    pub(crate) pane_id: Uuid,
    /// Most urgent status among the tab's live panes.
    pub(crate) status: PaneStatus,
    /// Every pane in the tab has exited.
    pub(crate) exited: bool,
}

/// Worker tabs keyed by their owning bot tab, in workstation and tab order.
pub(crate) fn bot_workers(
    snapshot: &SessionSnapshot,
    exited: impl Fn(Uuid) -> bool,
) -> HashMap<Uuid, Vec<BotWorker<'_>>> {
    let mut workers: HashMap<Uuid, Vec<BotWorker<'_>>> = HashMap::new();
    for workspace in snapshot
        .workspaces
        .iter()
        .filter(|workspace| !workspace.is_bots())
    {
        for tab in &workspace.tabs {
            let Some(owner) = tab.owner_bot else {
                continue;
            };
            let Some(&pane_id) = visible_panes(&tab.layout).first() else {
                continue;
            };
            let mut panes = Vec::new();
            collect_terminal_tabs(&tab.layout, &mut panes);
            let live = panes.iter().filter(|pane| !exited(pane.id));
            workers.entry(owner).or_default().push(BotWorker {
                workspace,
                tab,
                pane_id,
                status: max_pane_status(live.clone().map(|pane| pane.status)),
                exited: live.count() == 0,
            });
        }
    }
    workers
}

impl HhApp {
    pub(crate) fn bots_workspace(&self) -> Option<&Workspace> {
        self.session
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.is_bots())
    }

    pub(crate) fn bot_tab(&self, tab_id: Uuid) -> Option<&Tab> {
        self.bots_workspace()?
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id && tab.bot.is_some())
    }

    pub(crate) fn pane_is_bot(&self, pane_id: Uuid) -> bool {
        self.bots_workspace().is_some_and(|workspace| {
            workspace
                .tabs
                .iter()
                .any(|tab| find_pane(&tab.layout, pane_id).is_some())
        })
    }

    /// The active workspace unless it is the Bots workspace.
    pub(crate) fn active_workstation(&self) -> Option<Uuid> {
        let active = self.sidebar.active_workspace?;
        (self.bots_workspace().map(|workspace| workspace.id) != Some(active)).then_some(active)
    }

    fn bot_status_section(&self, tab: &Tab) -> Option<ActivitySection> {
        let pane = bot_pane(tab)?;
        let exited = self
            .session
            .pane_states
            .get(&pane.id)
            .is_some_and(|state| state.exited);
        activity_section(pane.status, exited)
    }

    /// Bots whose pane is waiting on the user; drives the robot badge.
    pub(crate) fn bots_needing_you(&self) -> usize {
        self.bots_workspace().map_or(0, |workspace| {
            workspace
                .tabs
                .iter()
                .filter(|tab| {
                    tab.bot.is_some()
                        && self.bot_status_section(tab) == Some(ActivitySection::NeedsYou)
                })
                .count()
        })
    }

    /// The bell and robot buttons. With Settings open they close it and show
    /// their view; otherwise a second click returns to the workstations.
    pub(crate) fn toggle_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        let next = if matches!(self.editor.modal, Modal::AppearanceSettings) {
            self.close_settings(cx);
            mode
        } else if self.sidebar.sidebar_mode == mode {
            SidebarMode::Workstations
        } else {
            mode
        };
        self.set_sidebar_mode(next, cx);
    }

    pub(crate) fn set_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        self.sidebar.sidebar_mode = mode;
        match mode {
            SidebarMode::Workstations => self.leave_bot_view(cx),
            SidebarMode::Notifications => {
                self.sidebar.sidebar_visible = true;
                self.refresh_notifications();
            }
            SidebarMode::Bots => {
                self.sidebar.sidebar_visible = true;
                if !self.coding_agents.loaded {
                    self.refresh_coding_agents(cx);
                }
            }
        }
        cx.notify();
    }

    /// Returns the main area from a bot to the workstation shown before it.
    fn leave_bot_view(&mut self, cx: &mut Context<Self>) {
        if self.active_workstation().is_some() || self.sidebar.active_workspace.is_none() {
            return;
        }
        let Some(snapshot) = self.session.snapshot.as_ref() else {
            return;
        };
        let mut workstations = snapshot
            .workspaces
            .iter()
            .filter(|workspace| !workspace.is_bots() && workspace_is_selectable(workspace))
            .collect::<Vec<_>>();
        workstations.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        let target = self
            .sidebar
            .return_workstation
            .filter(|id| workstations.iter().any(|workspace| workspace.id == *id))
            .or_else(|| workstations.first().map(|workspace| workspace.id));
        match target {
            Some(workspace_id) => self.select_workspace(workspace_id, cx),
            None => self.sidebar.active_workspace = None,
        }
    }

    fn remember_return_workstation(&mut self) {
        if let Some(workspace_id) = self.active_workstation() {
            self.sidebar.return_workstation = Some(workspace_id);
        }
    }

    /// Shows one bot's terminal in the main area.
    pub(crate) fn open_bot(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let Some((workspace_id, pane_id)) = self.bots_workspace().and_then(|workspace| {
            let tab = workspace.tabs.iter().find(|tab| tab.id == tab_id)?;
            Some((workspace.id, bot_pane(tab)?.id))
        }) else {
            return;
        };
        self.remember_return_workstation();
        self.editor.modal = Modal::None;
        self.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
    }

    /// Shows one of a bot's worker tabs; the sidebar stays on Bots so the
    /// next worker is one click away.
    pub(crate) fn open_bot_worker(
        &mut self,
        workspace_id: Uuid,
        tab_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.editor.modal = Modal::None;
        self.select_sidebar_pane(workspace_id, tab_id, pane_id, cx);
    }

    /// Shows a just-created bot, before its tab reaches the snapshot.
    pub(crate) fn show_bot_pane(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.remember_return_workstation();
        self.sidebar.sidebar_mode = SidebarMode::Bots;
        self.sidebar.sidebar_visible = true;
        self.focus_created_pane(workspace_id, pane_id, cx);
    }

    pub(crate) fn begin_bot_creation(&mut self, cx: &mut Context<Self>) {
        let configured = self.bot_settings().default_agent;
        self.editor.modal = Modal::WorkspaceCreation(WorkspaceCreationDialog::new_bot(
            default_bot_agent(configured, &self.coding_agents.agents),
        ));
        self.editor.workspace_input_layouts = [None, None, None, None];
        self.editor.workspace_input_bounds = [None, None, None, None];
        if !self.coding_agents.loaded {
            self.refresh_coding_agents(cx);
        }
        cx.notify();
    }

    pub(crate) fn set_bot_creation_agent(
        &mut self,
        agent: TerminalProfile,
        cx: &mut Context<Self>,
    ) {
        if let Some(dialog) = self.editor.modal.workspace_creation_mut() {
            dialog.agent = Some(agent);
            dialog.error = None;
            cx.notify();
        }
    }

    pub(crate) fn bot_settings(&self) -> BotSettings {
        self.session
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.bots.clone())
            .unwrap_or_default()
    }

    pub(crate) fn set_default_bot_agent(&mut self, agent: Option<TerminalProfile>) {
        let mut settings = self.bot_settings();
        settings.default_agent = agent;
        self.dispatch(ClientRequest::SetBotSettings { settings });
    }

    pub(crate) fn open_bot_menu(
        &mut self,
        tab_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.color_picker = None;
        self.editor.modal = Modal::BotMenu(BotMenu {
            tab_id,
            position,
            agents_open: false,
        });
        cx.notify();
    }

    pub(crate) fn toggle_bot_menu_agents(&mut self, cx: &mut Context<Self>) {
        if let Modal::BotMenu(menu) = &mut self.editor.modal {
            menu.agents_open = !menu.agents_open;
            if menu.agents_open && !self.coding_agents.loaded {
                self.refresh_coding_agents(cx);
            }
            cx.notify();
        }
    }

    pub(crate) fn begin_bot_rename(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        let Some(name) = self.bot_tab(tab_id).map(|tab| bot_name(tab).to_owned()) else {
            return;
        };
        self.editor.modal = Modal::GroupRename(GroupRenameEditor {
            tab_id,
            value: name,
            replace_on_type: true,
            bot: true,
        });
        cx.notify();
    }

    fn dispatch_bot_request(&mut self, request: ClientRequest, cx: &mut Context<Self>) {
        self.dispatch_with(
            request,
            Box::new(|this, cx, result| {
                match result {
                    Ok(ServiceResponse::Ack) => this.layout.last_sizes.clear(),
                    Ok(response) => this.report_unexpected(&response),
                    Err(error) => this.report(&error),
                }
                cx.notify();
            }),
        );
        cx.notify();
    }

    pub(crate) fn set_bot_agent(
        &mut self,
        tab_id: Uuid,
        agent: TerminalProfile,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_bot_request(ClientRequest::SetBotAgent { tab_id, agent }, cx);
    }

    pub(crate) fn restart_bot(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_bot_request(ClientRequest::RestartBot { tab_id }, cx);
    }

    /// Opens the folder picker and moves the bot's home to the chosen folder.
    pub(crate) fn begin_bot_home_edit(&mut self, tab_id: Uuid, cx: &mut Context<Self>) {
        self.editor.modal = Modal::None;
        self.prompt_local_directory(
            "Choose home folder",
            move |this, dir, cx| this.set_bot_home(tab_id, Some(dir), cx),
            cx,
        );
    }

    /// Sets the bot's home folder; `None` restores the default.
    pub(crate) fn set_bot_home(
        &mut self,
        tab_id: Uuid,
        home: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_bot_request(ClientRequest::SetBotHome { tab_id, home }, cx);
    }

    /// Re-runs login-PATH discovery on the service and stores the result. An
    /// open New bot dialog without an agent adopts the default once known.
    pub(crate) fn refresh_coding_agents(&mut self, cx: &mut Context<Self>) {
        self.coding_agents.loading = true;
        self.coding_agents.error = None;
        cx.notify();
        self.dispatch_with(
            ClientRequest::GetCodingAgents,
            Box::new(|this, cx, result| {
                let state = &mut this.coding_agents;
                state.loading = false;
                state.loaded = true;
                match result {
                    Ok(ServiceResponse::CodingAgents { agents }) => state.agents = agents,
                    Ok(other) => state.error = Some(format!("unexpected response: {other:?}")),
                    Err(error) => state.error = Some(format!("{error:#}")),
                }
                let default = default_bot_agent(
                    this.bot_settings().default_agent,
                    &this.coding_agents.agents,
                );
                if let Some(dialog) = this.editor.modal.workspace_creation_mut()
                    && dialog.agent.is_none()
                {
                    dialog.agent = default;
                }
                cx.notify();
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{bot_workers, default_bot_agent};
    use hh_protocol::{
        BotSpec, CodingAgent, Pane, PaneLayout, PaneStatus, SessionSnapshot, SplitAxis, Tab,
        TerminalProfile, WorkspaceKind,
    };
    use uuid::Uuid;

    fn pane(status: PaneStatus) -> Pane {
        let PaneLayout::Leaf { mut pane } = SessionSnapshot::seeded()
            .workspaces
            .remove(0)
            .tabs
            .remove(0)
            .layout
        else {
            unreachable!("the seeded tab is a single terminal")
        };
        pane.id = Uuid::new_v4();
        pane.status = status;
        pane
    }

    fn tab(owner_bot: Option<Uuid>, layout: PaneLayout) -> Tab {
        let mut tab = SessionSnapshot::seeded().workspaces[0].tabs[0].clone();
        tab.id = Uuid::new_v4();
        tab.owner_bot = owner_bot;
        tab.layout = layout;
        tab
    }

    #[test]
    fn bot_workers_group_owned_tabs_with_their_most_urgent_live_status() {
        let (bot_a, bot_b) = (Uuid::new_v4(), Uuid::new_v4());
        let running = pane(PaneStatus::Working);
        let waiting = pane(PaneStatus::NeedsApproval);
        let finished = pane(PaneStatus::Idle);
        let mut snapshot = SessionSnapshot::seeded();
        let workstation = &mut snapshot.workspaces[0];
        let split = PaneLayout::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneLayout::Leaf {
                pane: running.clone(),
            }),
            second: Box::new(PaneLayout::Leaf {
                pane: waiting.clone(),
            }),
        };
        workstation.tabs.push(tab(Some(bot_a), split));
        workstation.tabs.push(tab(
            Some(bot_b),
            PaneLayout::Leaf {
                pane: finished.clone(),
            },
        ));
        let mut bots = workstation.clone();
        bots.id = Uuid::new_v4();
        bots.kind = WorkspaceKind::Bots;
        let mut bot_tab = tab(
            Some(bot_a),
            PaneLayout::Leaf {
                pane: pane(PaneStatus::Idle),
            },
        );
        bot_tab.bot = Some(BotSpec {
            agent: TerminalProfile::Omp,
            instructions: None,
            home: None,
        });
        bots.tabs = vec![bot_tab];
        snapshot.workspaces.push(bots);

        let workers = bot_workers(&snapshot, |pane_id| pane_id == finished.id);

        assert_eq!(
            workers.len(),
            2,
            "unowned tabs and Bots-workspace tabs are not workers"
        );
        let [worker] = workers[&bot_a].as_slice() else {
            panic!("bot A owns exactly one worker tab")
        };
        assert_eq!(worker.status, PaneStatus::NeedsApproval);
        assert!(!worker.exited);
        assert_eq!(
            worker.pane_id, running.id,
            "opening a worker focuses its visible pane"
        );
        let [done] = workers[&bot_b].as_slice() else {
            panic!("bot B owns exactly one worker tab")
        };
        assert!(done.exited);
    }

    fn agent(profile: TerminalProfile) -> CodingAgent {
        CodingAgent {
            profile,
            command: String::new(),
            path: String::new(),
        }
    }

    #[test]
    fn new_bots_default_to_the_configured_agent_then_omp_then_the_first_installed() {
        let installed = [
            agent(TerminalProfile::Claude),
            agent(TerminalProfile::Omp),
            agent(TerminalProfile::Codex),
        ];
        assert_eq!(
            default_bot_agent(Some(TerminalProfile::Codex), &installed),
            Some(TerminalProfile::Codex)
        );
        assert_eq!(
            default_bot_agent(None, &installed),
            Some(TerminalProfile::Omp)
        );
        assert_eq!(
            default_bot_agent(Some(TerminalProfile::Hermes), &installed),
            Some(TerminalProfile::Omp),
            "an uninstalled default falls back"
        );
        assert_eq!(
            default_bot_agent(None, &installed[..1]),
            Some(TerminalProfile::Claude)
        );
        assert_eq!(default_bot_agent(Some(TerminalProfile::Omp), &[]), None);
    }
}
