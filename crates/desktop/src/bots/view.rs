//! Bot surfaces: the main-area bot view, its context menu, and the New bot
//! agent picker.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext as _, Context, InteractiveElement, IntoElement, ParentElement, Pixels,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use hh_protocol::{Tab, TerminalProfile, Workspace};

use super::{bot_name, bot_pane};
use crate::helpers::{element_key, find_pane, render_terminal_profile_icon};
use crate::menus::{anchored_menu, menu_separator};
use crate::view_models::{BotMenu, BotThreadMenu, Modal, TooltipView};
use crate::{HhApp, THEME, WORKSPACE_TAB_STRIP_HEIGHT};

impl HhApp {
    /// The Bots workspace in the main area: a slim header over the selected
    /// bot's terminal, without a tab strip.
    pub(crate) fn render_bot_view(
        &self,
        workspace: &Workspace,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab = self
            .layout
            .focused_pane
            .and_then(|pane_id| {
                workspace
                    .tabs
                    .iter()
                    .find(|tab| find_pane(&tab.layout, pane_id).is_some())
            })
            .or_else(|| workspace.tabs.first())
            .filter(|tab| tab.bot.is_some());
        let header = div()
            .h(px(WORKSPACE_TAB_STRIP_HEIGHT))
            .flex_none()
            .px(px(12.0))
            .border_b_1()
            .border_color(rgb(THEME.border_strong))
            .bg(rgb(THEME.surface))
            .flex()
            .items_center()
            .gap(px(8.0))
            .font_family(".SystemUIFont");
        let header = match tab {
            Some(tab) => {
                let tab_id = tab.id;
                let agent = tab.bot.as_ref().map(|bot| bot.agent).unwrap_or_default();
                let home = format!("Home: {}", bot_home_label(tab));
                header
                    .child(render_terminal_profile_icon(agent, THEME.muted, 18.0))
                    .child(
                        div()
                            .min_w(px(0.0))
                            .truncate()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child(bot_name(tab).to_owned()),
                    )
                    .child(
                        div()
                            .id(("bot-agent", element_key(tab_id)))
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .tooltip(move |_, cx| {
                                cx.new(|_| TooltipView { text: home.clone() }).into()
                            })
                            .child(agent.display_name()),
                    )
                    .child(
                        div()
                            .id(("restart-bot", element_key(tab_id)))
                            .flex_none()
                            .px(px(9.0))
                            .py(px(3.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(THEME.border))
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.border_color(rgb(THEME.accent)))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.restart_bot(tab_id, cx)),
                            )
                            .child("Restart"),
                    )
            }
            None => header.child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(THEME.foreground))
                    .child("Bots"),
            ),
        };
        let content = match tab.and_then(bot_pane) {
            Some(pane) => self.render_terminal(std::slice::from_ref(pane), pane.id, false, cx),
            None => self.render_no_bot_selected(cx),
        };
        div()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .h_full()
            .flex_1()
            .bg(rgb(THEME.terminal))
            .flex()
            .flex_col()
            .child(header)
            .child(div().min_h(px(0.0)).flex_1().child(content))
            .into_any_element()
    }

    fn render_no_bot_selected(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .font_family(".SystemUIFont")
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(THEME.muted))
                    .child("Pick a bot in the sidebar, or create one."),
            )
            .child(
                div()
                    .id("empty-bots-new-bot")
                    .px(px(16.0))
                    .py(px(8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(rgb(THEME.accent))
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(rgb(0xffffff))
                    .on_click(cx.listener(|this, _, _, cx| this.begin_bot_creation(cx)))
                    .child("New bot"),
            )
            .into_any_element()
    }

    pub(crate) fn render_bot_menu(
        &self,
        menu: BotMenu,
        menu_max_height: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = menu.tab_id;
        let key = element_key(tab_id);
        let bot = self.bot_tab(tab_id).and_then(|tab| tab.bot.as_ref());
        let current_agent = bot.map(|bot| bot.agent);
        let custom_home = bot.is_some_and(|bot| bot.home.is_some());
        let agents: Vec<AnyElement> = if !menu.agents_open {
            Vec::new()
        } else if self.coding_agents.loading {
            vec![Self::bot_menu_note("Scanning your login PATH…")]
        } else if self.coding_agents.agents.is_empty() {
            vec![Self::bot_menu_note(
                "No coding agent CLIs were found on your login PATH",
            )]
        } else {
            self.coding_agents
                .agents
                .iter()
                .enumerate()
                .map(|(index, agent)| {
                    let profile = agent.profile;
                    let current = current_agent == Some(profile);
                    div()
                        .id(("bot-menu-agent", index))
                        .mx(px(5.0))
                        .pl(px(22.0))
                        .pr(px(9.0))
                        .py(px(6.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.editor.modal = Modal::None;
                            if !current {
                                this.set_bot_agent(tab_id, profile, cx);
                            }
                            cx.notify();
                        }))
                        .child(render_terminal_profile_icon(profile, THEME.muted, 16.0))
                        .child(div().flex_1().child(profile.display_name()))
                        .when(current, |element| element.child("✓"))
                        .into_any_element()
                })
                .collect()
        };
        anchored_menu(
            menu.position,
            div()
                .id(("bot-context-menu", key))
                .w(px(232.0))
                .max_h(menu_max_height)
                .overflow_y_scroll()
                .py(px(5.0))
                .rounded(px(7.0))
                .bg(rgb(THEME.elevated))
                .border_1()
                .border_color(rgb(THEME.border_strong))
                .shadow_lg()
                .occlude()
                .child(self.create_menu_item(
                    ("rename-bot-menu", key),
                    "Rename…",
                    cx,
                    move |this, cx| this.begin_bot_rename(tab_id, cx),
                ))
                .child(
                    div()
                        .id(("change-bot-agent-menu", key))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.foreground))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_bot_menu_agents(cx)))
                        .flex()
                        .items_center()
                        .child(div().flex_1().child("Change agent"))
                        .child(if menu.agents_open { "⌄" } else { "▸" }),
                )
                .children(agents)
                .child(self.create_menu_item(
                    ("restart-bot-menu", key),
                    "Restart",
                    cx,
                    move |this, cx| this.restart_bot(tab_id, cx),
                ))
                .child(self.create_menu_item(
                    ("set-bot-home-menu", key),
                    "Set home folder…",
                    cx,
                    move |this, cx| this.begin_bot_home_edit(tab_id, cx),
                ))
                .when(custom_home, |element| {
                    element.child(self.create_menu_item(
                        ("default-bot-home-menu", key),
                        "Use default home folder",
                        cx,
                        move |this, cx| {
                            this.editor.modal = Modal::None;
                            this.set_bot_home(tab_id, None, cx);
                        },
                    ))
                })
                .child(menu_separator())
                .child(
                    div()
                        .id(("delete-bot-menu", key))
                        .mx(px(5.0))
                        .px(px(9.0))
                        .py(px(7.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .font_family(".SystemUIFont")
                        .text_sm()
                        .text_color(rgb(THEME.danger))
                        .hover(|element| element.bg(rgb(THEME.accent_soft)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.begin_tab_close(tab_id, cx);
                        }))
                        .child("Delete…"),
                ),
        )
    }

    /// Right-click menu of a thread row: pin or unpin it.
    pub(crate) fn render_bot_thread_menu(
        &self,
        menu: &BotThreadMenu,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (tab_id, pinned) = (menu.tab_id, menu.pinned);
        let thread_id = menu.thread_id.clone();
        anchored_menu(
            menu.position,
            div()
                .id(("bot-thread-menu", element_key(tab_id)))
                .w(px(180.0))
                .py(px(5.0))
                .rounded(px(7.0))
                .bg(rgb(THEME.elevated))
                .border_1()
                .border_color(rgb(THEME.border_strong))
                .shadow_lg()
                .occlude()
                .child(self.create_menu_item(
                    "pin-bot-thread-menu",
                    if pinned { "Unpin" } else { "Pin" },
                    cx,
                    move |this, cx| {
                        this.set_bot_thread_pinned(tab_id, thread_id.clone(), !pinned, cx);
                    },
                )),
        )
    }

    fn bot_menu_note(text: &'static str) -> AnyElement {
        div()
            .mx(px(5.0))
            .pl(px(22.0))
            .pr(px(9.0))
            .py(px(6.0))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.dim))
            .child(text)
            .into_any_element()
    }

    /// Installed agents as selectable chips for the New bot dialog.
    pub(crate) fn render_bot_agent_picker(
        &self,
        selected: Option<TerminalProfile>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = &self.coding_agents;
        if state.agents.is_empty() {
            let note = if state.loading || !state.loaded {
                "Scanning your login PATH…".to_owned()
            } else if let Some(error) = state.error.as_ref() {
                error.clone()
            } else {
                "No coding agent CLIs were found on your login PATH. Install omp, Claude Code, Codex, Hermes, or another supported agent.".to_owned()
            };
            return div()
                .font_family(".SystemUIFont")
                .text_sm()
                .text_color(rgb(THEME.muted))
                .child(note)
                .into_any_element();
        }
        div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .children(state.agents.iter().enumerate().map(|(index, agent)| {
                let profile = agent.profile;
                let active = selected == Some(profile);
                div()
                    .id(("new-bot-agent", index))
                    .px(px(9.0))
                    .py(px(5.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(if active {
                        THEME.accent
                    } else {
                        THEME.border_strong
                    }))
                    .bg(rgb(if active {
                        THEME.accent_soft
                    } else {
                        THEME.surface
                    }))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .font_family(".SystemUIFont")
                    .text_sm()
                    .text_color(rgb(THEME.foreground))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_bot_creation_agent(profile, cx)),
                    )
                    .child(render_terminal_profile_icon(profile, THEME.muted, 16.0))
                    .child(profile.display_name())
            }))
            .into_any_element()
    }
}

/// The folder a bot's terminal starts in: its custom home, else the default
/// `<state>/bots/<tab id>`.
fn bot_home_label(tab: &Tab) -> String {
    match tab.bot.as_ref().and_then(|bot| bot.home.clone()) {
        Some(home) => home,
        None => hh_protocol::state_directory().map_or_else(
            || "default bot folder".to_owned(),
            |state| {
                state
                    .join("bots")
                    .join(tab.id.to_string())
                    .display()
                    .to_string()
            },
        ),
    }
}
