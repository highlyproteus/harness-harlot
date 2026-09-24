//! Bot menus: the bot card's context menu, a saved thread's pin menu, and
//! the New bot agent picker.
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, InteractiveElement, IntoElement, ParentElement, Pixels,
    StatefulInteractiveElement, Styled, div, px, rgb,
};
use hh_protocol::TerminalProfile;

use crate::helpers::{element_key, render_terminal_profile_icon};
use crate::menus::{anchored_menu, menu_separator};
use crate::view_models::{BotMenu, BotThreadMenu, Modal};
use crate::{HhApp, THEME};

impl HhApp {
    pub(crate) fn render_bot_menu(
        &self,
        menu: BotMenu,
        menu_max_height: Pixels,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bot_id = menu.bot_id;
        let key = element_key(bot_id);
        let bot = self.bot_spec(bot_id);
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
                                this.set_bot_agent(bot_id, profile, cx);
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
                    move |this, cx| this.begin_bot_rename(bot_id, cx),
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
                    move |this, cx| this.restart_bot(bot_id, cx),
                ))
                .child(self.create_menu_item(
                    ("set-bot-home-menu", key),
                    "Set home folder…",
                    cx,
                    move |this, cx| this.begin_bot_home_edit(bot_id, cx),
                ))
                .when(custom_home, |element| {
                    element.child(self.create_menu_item(
                        ("default-bot-home-menu", key),
                        "Use default home folder",
                        cx,
                        move |this, cx| {
                            this.editor.modal = Modal::None;
                            this.set_bot_home(bot_id, None, cx);
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
                            this.begin_bot_delete(bot_id, cx);
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
        let (bot_id, pinned) = (menu.bot_id, menu.pinned);
        let thread_id = menu.thread_id.clone();
        anchored_menu(
            menu.position,
            div()
                .id(("bot-thread-menu", element_key(bot_id)))
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
                        this.set_bot_thread_pinned(bot_id, thread_id.clone(), !pinned, cx);
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
