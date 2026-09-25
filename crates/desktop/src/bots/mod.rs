//! Bots: coding-agent CLIs, each running in its own `WorkspaceKind::Bot`
//! workspace whose tabs are the bot's live threads. Bots never appear among
//! workstations; the sidebar's Bots mode lists and opens them.
use gpui::{Context, Pixels, Point};
use hh_protocol::{
    BotSettings, BotSpec, ClientRequest, CodingAgent, ServiceResponse, TerminalProfile, Workspace,
};
use uuid::Uuid;

use crate::HhApp;
use crate::helpers::{find_pane, visible_panes, workspace_is_selectable};
use crate::notifications::bots_needing_you;
use crate::view_models::{
    BotMenu, Modal, SidebarMode, WorkspaceCreationDialog, WorkspaceDeleteConfirmation,
    WorkspaceRenameEditor,
};

mod menus;
mod settings;
mod threads;

pub(crate) use threads::{BotThreadsState, NEW_THREAD_TITLE, now_ms, relative_time, saved_threads};

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

/// The pane a bot opens on: its most recently activated live thread pane,
/// else the first visible pane of its first tab. Returns `(tab id, pane id)`.
pub(crate) fn bot_entry_pane(workspace: &Workspace) -> Option<(Uuid, Uuid)> {
    let locate = |pane_id: Uuid| {
        workspace
            .tabs
            .iter()
            .find(|tab| find_pane(&tab.layout, pane_id).is_some())
            .map(|tab| (tab.id, pane_id))
    };
    workspace
        .bot
        .iter()
        .flat_map(|bot| &bot.thread_panes)
        .filter(|(_, thread)| thread.activated_ms > 0)
        .filter_map(|(pane_id, thread)| Some((thread.activated_ms, locate(*pane_id)?)))
        .max_by_key(|(activated_ms, _)| *activated_ms)
        .map(|(_, entry)| entry)
        .or_else(|| {
            let tab = workspace.tabs.first()?;
            Some((tab.id, *visible_panes(&tab.layout).first()?))
        })
}

impl HhApp {
    /// Bot workspaces in sidebar order: pinned first, then by `order`.
    pub(crate) fn bot_workspaces(&self) -> Vec<&Workspace> {
        let mut bots = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .workspaces
                    .iter()
                    .filter(|workspace| workspace.is_bot())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        bots.sort_by_key(|workspace| (!workspace.pinned, workspace.order));
        bots
    }

    pub(crate) fn bot_workspace(&self, bot_id: Uuid) -> Option<&Workspace> {
        self.session
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == bot_id && workspace.is_bot())
    }

    pub(crate) fn bot_spec(&self, bot_id: Uuid) -> Option<&BotSpec> {
        self.bot_workspace(bot_id)?.bot.as_ref()
    }

    pub(crate) fn workspace_is_bot(&self, workspace_id: Uuid) -> bool {
        self.bot_workspace(workspace_id).is_some()
    }

    /// The bot workspace containing the pane.
    pub(crate) fn bot_for_pane(&self, pane_id: Uuid) -> Option<Uuid> {
        self.session
            .snapshot
            .as_ref()?
            .workspaces
            .iter()
            .filter(|workspace| workspace.is_bot())
            .find(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .any(|tab| find_pane(&tab.layout, pane_id).is_some())
            })
            .map(|workspace| workspace.id)
    }

    pub(crate) fn pane_is_bot(&self, pane_id: Uuid) -> bool {
        self.bot_for_pane(pane_id).is_some()
    }

    /// The active workspace unless it is a bot.
    pub(crate) fn active_workstation(&self) -> Option<Uuid> {
        let active = self.sidebar.active_workspace?;
        (!self.workspace_is_bot(active)).then_some(active)
    }

    /// Bots with a pane waiting on the user; drives the robot badge.
    pub(crate) fn bots_needing_you(&self) -> usize {
        self.session.snapshot.as_ref().map_or(0, |snapshot| {
            bots_needing_you(snapshot, &self.session.pane_states)
        })
    }

    /// The hammer, robot, and bell toolbar buttons; see `next_sidebar_mode`.
    pub(crate) fn toggle_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        let settings_open = matches!(self.editor.modal, Modal::AppearanceSettings);
        if settings_open {
            self.close_settings(cx);
        }
        let next = next_sidebar_mode(
            self.sidebar.sidebar_mode,
            mode,
            settings_open,
            self.sidebar.notifications_return,
        );
        self.set_sidebar_mode(next, cx);
    }

    pub(crate) fn set_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        if mode == SidebarMode::Notifications && self.sidebar.sidebar_mode != mode {
            self.sidebar.notifications_return = self.sidebar.sidebar_mode;
        }
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
                self.start_bot_threads_refresh(cx);
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
            .filter(|workspace| !workspace.is_bot() && workspace_is_selectable(workspace))
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

    /// Shows a bot in the main area on its most recently used thread.
    pub(crate) fn open_bot(&mut self, bot_id: Uuid, cx: &mut Context<Self>) {
        let Some(workspace) = self.bot_workspace(bot_id) else {
            return;
        };
        let entry = bot_entry_pane(workspace);
        self.remember_return_workstation();
        self.editor.modal = Modal::None;
        match entry {
            Some((tab_id, pane_id)) => self.select_sidebar_pane(bot_id, tab_id, pane_id, cx),
            None => self.select_workspace(bot_id, cx),
        }
        self.sidebar.expanded_workspaces.insert(bot_id);
        self.refresh_bot_threads(bot_id);
    }

    /// Shows one pane of a bot (e.g. from Notifications) with the sidebar in
    /// Bots mode.
    pub(crate) fn open_bot_pane(
        &mut self,
        bot_id: Uuid,
        tab_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.remember_return_workstation();
        self.editor.modal = Modal::None;
        self.sidebar.expanded_workspaces.insert(bot_id);
        if self.sidebar.sidebar_mode != SidebarMode::Bots {
            self.set_sidebar_mode(SidebarMode::Bots, cx);
        }
        self.select_sidebar_pane(bot_id, tab_id, pane_id, cx);
    }

    /// Shows a just-created bot, before its workspace reaches the snapshot.
    pub(crate) fn show_bot_pane(
        &mut self,
        workspace_id: Uuid,
        pane_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        self.remember_return_workstation();
        self.sidebar.sidebar_mode = SidebarMode::Bots;
        self.sidebar.sidebar_visible = true;
        self.bot_threads.activated = Some(pane_id);
        self.focus_created_pane(workspace_id, pane_id, cx);
        self.start_bot_threads_refresh(cx);
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
        bot_id: Uuid,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.editor.color_picker = None;
        self.editor.modal = Modal::BotMenu(BotMenu {
            bot_id,
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

    /// Renames the bot: its workspace title is its name.
    pub(crate) fn begin_bot_rename(&mut self, bot_id: Uuid, cx: &mut Context<Self>) {
        let Some(name) = self
            .bot_workspace(bot_id)
            .map(|workspace| workspace.title.clone())
        else {
            return;
        };
        self.editor.modal = Modal::WorkspaceRename(WorkspaceRenameEditor {
            workspace_id: bot_id,
            value: name,
            replace_on_type: true,
            bot: true,
        });
        cx.notify();
    }

    /// Asks before deleting the bot workspace with all its threads.
    pub(crate) fn begin_bot_delete(&mut self, bot_id: Uuid, cx: &mut Context<Self>) {
        let Some(workspace) = self.bot_workspace(bot_id) else {
            return;
        };
        self.editor.modal = Modal::WorkspaceDelete(WorkspaceDeleteConfirmation {
            workspace_id: bot_id,
            title: workspace.title.clone(),
            active_terminal_count: workspace.active_terminal_count,
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
        bot_id: Uuid,
        agent: TerminalProfile,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_bot_request(ClientRequest::SetBotAgent { bot_id, agent }, cx);
    }

    /// Restarts the bot's active thread.
    pub(crate) fn restart_bot(&mut self, bot_id: Uuid, cx: &mut Context<Self>) {
        self.dispatch_bot_request(ClientRequest::RestartBot { bot_id }, cx);
    }

    /// Opens the folder picker and moves the bot's home to the chosen folder.
    pub(crate) fn begin_bot_home_edit(&mut self, bot_id: Uuid, cx: &mut Context<Self>) {
        self.editor.modal = Modal::None;
        self.prompt_local_directory(
            "Choose home folder",
            move |this, dir, cx| this.set_bot_home(bot_id, Some(dir), cx),
            cx,
        );
    }

    /// Sets the bot's home folder; `None` restores the default.
    pub(crate) fn set_bot_home(
        &mut self,
        bot_id: Uuid,
        home: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_bot_request(ClientRequest::SetBotHome { bot_id, home }, cx);
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

/// The view a toolbar mode button shows. With Settings open it shows its own
/// view. Otherwise a second click turns the view off: Notifications returns to
/// the view it was opened from, and Bots to Workstations, the home view the
/// hammer always lands on.
fn next_sidebar_mode(
    current: SidebarMode,
    clicked: SidebarMode,
    settings_open: bool,
    notifications_return: SidebarMode,
) -> SidebarMode {
    match (settings_open || current != clicked, clicked) {
        (true, _) => clicked,
        (false, SidebarMode::Notifications) => notifications_return,
        (false, _) => SidebarMode::Workstations,
    }
}

#[cfg(test)]
mod tests {
    use super::{bot_entry_pane, default_bot_agent, next_sidebar_mode};
    use crate::view_models::SidebarMode;
    use hh_protocol::{
        BotSpec, BotThreadPane, CodingAgent, PaneLayout, SessionSnapshot, TerminalProfile,
    };
    use uuid::Uuid;

    #[test]
    fn the_hammer_always_shows_workstations_and_other_modes_toggle_back() {
        use SidebarMode::{Bots, Notifications, Workstations};
        for current in [Workstations, Notifications, Bots] {
            for settings_open in [false, true] {
                assert_eq!(
                    next_sidebar_mode(current, Workstations, settings_open, Bots),
                    Workstations
                );
            }
        }
        assert_eq!(
            next_sidebar_mode(Workstations, Bots, false, Workstations),
            Bots
        );
        assert_eq!(
            next_sidebar_mode(Bots, Bots, false, Workstations),
            Workstations
        );
        // Notifications goes back to the view it was opened from.
        assert_eq!(
            next_sidebar_mode(Notifications, Notifications, false, Bots),
            Bots
        );
        assert_eq!(
            next_sidebar_mode(Notifications, Notifications, false, Workstations),
            Workstations
        );
        // Closing Settings shows the clicked view even if it was already the mode.
        assert_eq!(next_sidebar_mode(Bots, Bots, true, Workstations), Bots);
    }

    #[test]
    fn a_bot_opens_on_its_most_recently_activated_live_thread_else_its_first_tab() {
        let mut bot = SessionSnapshot::seeded().workspaces.remove(0);
        let template = bot.tabs[0].clone();
        let tabs = (1..=3_u128)
            .map(|index| {
                let mut tab = template.clone();
                tab.id = Uuid::from_u128(index);
                let PaneLayout::Leaf { pane } = &mut tab.layout else {
                    unreachable!("the seeded tab is a single terminal");
                };
                pane.id = Uuid::from_u128(index + 10);
                tab
            })
            .collect::<Vec<_>>();
        bot.tabs = tabs;
        let pane = |index: u128| Uuid::from_u128(index + 10);
        let activated = |ms: u64| BotThreadPane {
            session: None,
            activated_ms: ms,
        };
        let mut spec = BotSpec {
            agent: TerminalProfile::Omp,
            instructions: None,
            home: None,
            pinned_threads: Vec::new(),
            thread_panes: [(pane(1), activated(0)), (pane(2), activated(0))].into(),
        };
        bot.bot = Some(spec.clone());
        assert_eq!(
            bot_entry_pane(&bot),
            Some((Uuid::from_u128(1), pane(1))),
            "never-activated threads open the first tab"
        );
        spec.thread_panes = [
            (pane(1), activated(5)),
            (pane(3), activated(9)),
            // Evicted pane still recorded: it must not win.
            (Uuid::from_u128(99), activated(50)),
        ]
        .into();
        bot.bot = Some(spec);
        assert_eq!(
            bot_entry_pane(&bot),
            Some((Uuid::from_u128(3), pane(3))),
            "the newest activation among panes still in the tabs wins"
        );
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
