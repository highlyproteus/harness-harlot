//! The Bots sidebar: one row per bot, plus the New bot button.
use crate::bots::{bot_name, bot_pane};
use crate::helpers::{element_key, render_terminal_profile_icon};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME, pane_status_color};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, px, rgb,
};
use hh_protocol::Tab;

impl HhApp {
    pub(crate) fn render_sidebar_bots(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self
            .bots_workspace()
            .map(|workspace| {
                let showing_bots = self.sidebar.active_workspace == Some(workspace.id);
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab.bot.is_some())
                    .map(|tab| self.render_bot_row(tab, showing_bots, cx))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let empty = rows.is_empty();
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(34.0))
                    .px(px(12.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(rgb(THEME.foreground))
                            .child("Bots"),
                    )
                    .child(
                        div()
                            .id("new-bot")
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .border_1()
                            .border_color(rgb(THEME.border))
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.border_color(rgb(THEME.accent)))
                            .on_click(cx.listener(|this, _, _, cx| this.begin_bot_creation(cx)))
                            .child("＋ New bot"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-bots")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .children(rows)
                    .when(empty, |element| {
                        element.child(
                            div()
                                .px(px(12.0))
                                .py(px(6.0))
                                .font_family(".SystemUIFont")
                                .text_xs()
                                .text_color(rgb(THEME.dim))
                                .child(
                                    "No bots yet. A bot is an agent you talk to; it opens and watches worker tabs in your workstations.",
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_bot_row(&self, tab: &Tab, showing_bots: bool, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = tab.id;
        let agent = tab.bot.as_ref().map(|bot| bot.agent).unwrap_or_default();
        let pane = bot_pane(tab);
        let selected =
            showing_bots && pane.is_some_and(|pane| self.layout.focused_pane == Some(pane.id));
        let exited = pane.is_some_and(|pane| {
            self.session
                .pane_states
                .get(&pane.id)
                .is_some_and(|state| state.exited)
        });
        let status = pane.map(|pane| pane.status).unwrap_or_default();
        let dot = if exited {
            THEME.dim
        } else {
            pane_status_color(status).unwrap_or(THEME.border_strong)
        };
        let status_label = crate::notifications::activity_badge(status, exited);
        div()
            .id(("bot-row", element_key(tab_id)))
            .mx(px(6.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(8.0))
            .when(selected, |element| {
                element
                    .bg(rgb(THEME.accent_soft))
                    .border_1()
                    .border_color(rgb(THEME.accent))
            })
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_bot(tab_id, cx);
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_bot_menu(tab_id, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(render_terminal_profile_icon(agent, THEME.muted, 20.0))
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .child(bot_name(tab).to_owned()),
                    )
                    .child(
                        div()
                            .truncate()
                            .font_family(".SystemUIFont")
                            .text_xs()
                            .text_color(rgb(THEME.dim))
                            .child(agent.display_name()),
                    ),
            )
            .child(
                div()
                    .id(("bot-status", element_key(tab_id)))
                    .flex_none()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(rgb(dot))
                    .tooltip(move |_, cx| {
                        cx.new(|_| TooltipView {
                            text: status_label.to_owned(),
                        })
                        .into()
                    }),
            )
            .into_any_element()
    }
}
