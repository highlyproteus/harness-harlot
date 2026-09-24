//! Bots: coding-agent CLIs running in the reserved Bots workspace. Bots never
//! appear among workstations; the sidebar's Bots mode lists and opens them.
use gpui::{Context, Pixels, Point};
use hh_protocol::{
    BotSettings, ClientRequest, CodingAgent, Pane, ServiceResponse, Tab, TerminalProfile, Workspace,
};
use uuid::Uuid;

use crate::HhApp;
use crate::helpers::{find_pane, visible_panes, workspace_is_selectable};
use crate::notifications::{ActivitySection, activity_section};
use crate::view_models::{BotMenu, GroupRenameEditor, Modal, SidebarMode, WorkspaceCreationDialog};

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

    pub(crate) fn toggle_sidebar_mode(&mut self, mode: SidebarMode, cx: &mut Context<Self>) {
        let next = if self.sidebar.sidebar_mode == mode {
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
    use super::default_bot_agent;
    use hh_protocol::{CodingAgent, TerminalProfile};

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
