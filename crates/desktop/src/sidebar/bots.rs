//! The Bots sidebar: one workstation-style card per bot workspace (its live
//! thread tabs are ordinary tab rows), followed by the bot's saved threads.
use crate::bots::{now_ms, relative_time, saved_threads};
use crate::helpers::{element_key, render_terminal_profile_icon};
use crate::tab_chrome::{PaneIndicator, render_pane_indicator};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, StatefulInteractiveElement, Styled, div, linear_color_stop,
    linear_gradient, px, rgb, rgba,
};
use hh_protocol::{BotThread, TerminalProfile};
use uuid::Uuid;

/// Height of one saved-thread row, the same as a tab row.
const THREAD_ROW_HEIGHT: f32 = 27.0;
/// Saved threads shown before the list scrolls and fades at the bottom.
const THREAD_ROWS_VISIBLE: usize = 15;
/// Height of `THREAD_ROWS_VISIBLE` rows.
const THREAD_LIST_MAX_HEIGHT: f32 = THREAD_ROW_HEIGHT * 15.0;

impl HhApp {
    pub(crate) fn render_sidebar_bots(&self, cx: &mut Context<Self>) -> AnyElement {
        let cards = self
            .bot_workspaces()
            .into_iter()
            .enumerate()
            .map(|(index, workspace)| self.render_workspace_section(index, workspace, cx))
            .collect::<Vec<_>>();
        let empty = cards.is_empty();
        div()
            .min_h(px(0.0))
            .flex_1()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(34.0))
                    .pl(px(12.0))
                    .pr(px(8.0))
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
                            .flex_none()
                            .w(px(22.0))
                            .h(px(22.0))
                            .rounded(px(5.0))
                            .cursor_pointer()
                            .bg(rgb(THEME.surface))
                            .border_1()
                            .border_color(rgb(THEME.border))
                            .font_family(".SystemUIFont")
                            .text_sm()
                            .text_color(rgb(THEME.foreground))
                            .hover(|element| element.border_color(rgb(THEME.accent)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(|_, cx| {
                                cx.new(|_| TooltipView {
                                    text: "New bot".to_owned(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.begin_bot_creation(cx)))
                            .child("＋"),
                    ),
            )
            .child(
                div()
                    .id("sidebar-bots")
                    .min_h(px(0.0))
                    .flex_1()
                    .overflow_y_scroll()
                    .children(cards)
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

    /// Right-click or "⋮" on a card: the bot menu for bots, else the
    /// workstation menu.
    pub(crate) fn open_card_menu(
        &mut self,
        workspace_id: Uuid,
        bot: bool,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if bot {
            self.open_bot_menu(workspace_id, position, cx);
        } else {
            self.open_workspace_menu(workspace_id, position, cx);
        }
    }

    /// The ＋ on a bot card header: starts a new thread tab.
    pub(crate) fn render_new_thread_button(
        &self,
        bot_id: Uuid,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(("new-bot-thread", element_key(bot_id)))
            .flex_none()
            .w(px(16.0))
            .h(px(18.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .font_family(".SystemUIFont")
            .text_sm()
            .text_color(rgb(THEME.dim))
            .hover(|element| element.text_color(rgb(THEME.foreground)))
            .tooltip(|_, cx| {
                cx.new(|_| TooltipView {
                    text: "New thread".to_owned(),
                })
                .into()
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_bot_thread(bot_id, None, cx);
                cx.stop_propagation();
            }))
            .child("＋")
            .into_any_element()
    }

    /// The bot's saved threads as tab-style rows below its live tabs,
    /// scrolling with a bottom fade past `THREAD_ROWS_VISIBLE` rows.
    pub(crate) fn render_saved_thread_rows(
        &self,
        bot_id: Uuid,
        agent: TerminalProfile,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let threads = saved_threads(self.bot_threads.lists.get(&bot_id)?);
        if threads.is_empty() {
            return None;
        }
        let now = now_ms();
        let overflows = threads.len() > THREAD_ROWS_VISIBLE;
        let rows = threads
            .into_iter()
            .map(|thread| self.render_saved_thread_row(bot_id, agent, thread, now, cx))
            .collect::<Vec<_>>();
        let fade = THEME.sidebar;
        Some(
            div()
                .relative()
                .child(
                    div()
                        .id(("bot-saved-threads", element_key(bot_id)))
                        .max_h(px(THREAD_LIST_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(rows),
                )
                .when(overflows, |element| {
                    element.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(THREAD_ROW_HEIGHT))
                            .bg(linear_gradient(
                                180.0,
                                linear_color_stop(rgba(fade << 8), 0.0),
                                linear_color_stop(rgb(fade), 1.0),
                            )),
                    )
                })
                .into_any_element(),
        )
    }

    /// One saved thread: agent icon, title, pin, age, and a × that deletes
    /// it. Clicking resumes it in a new tab; right-click pins or unpins it.
    fn render_saved_thread_row(
        &self,
        bot_id: Uuid,
        agent: TerminalProfile,
        thread: &BotThread,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let title = thread
            .title
            .clone()
            .unwrap_or_else(|| crate::bots::NEW_THREAD_TITLE.to_owned());
        let open_id = thread.id.clone();
        let delete_id = thread.id.clone();
        let delete_title = title.clone();
        let menu_thread = thread.clone();
        div()
            .id(gpui::ElementId::Name(
                format!("bot-saved-thread-{}", thread.id).into(),
            ))
            .ml(px(20.0))
            .mr(px(4.0))
            .px(px(7.0))
            .h(px(THREAD_ROW_HEIGHT))
            .flex_none()
            .rounded(px(4.0))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(7.0))
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.foreground))
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_bot_thread(bot_id, Some(open_id.clone()), cx);
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_bot_thread_menu(bot_id, &menu_thread, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(render_terminal_profile_icon(agent, THEME.foreground, 13.0)),
            )
            .child(div().min_w(px(0.0)).flex_1().truncate().child(title))
            .when(thread.pinned, |element| {
                element.child(div().flex_none().child("📌"))
            })
            .child(
                div()
                    .flex_none()
                    .child(relative_time(now, thread.updated_ms)),
            )
            // A saved thread has no pane, so its status slot stays empty.
            .child(render_pane_indicator(PaneIndicator::None))
            .child(self.render_close_button(
                gpui::ElementId::Name(format!("delete-saved-thread-{}", thread.id).into()),
                THEME.foreground,
                "Delete thread…".to_owned(),
                move |this, cx| {
                    this.begin_bot_thread_delete(
                        bot_id,
                        delete_id.clone(),
                        delete_title.clone(),
                        cx,
                    );
                },
                cx,
            ))
            .into_any_element()
    }
}
