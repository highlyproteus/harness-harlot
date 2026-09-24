//! The Bots sidebar: one card per bot with its threads (omp bots) and chips
//! for its worker tabs, plus the New bot button.
use crate::bots::{BotWorker, bot_name, bot_pane, bot_workers, has_threads, now_ms, relative_time};
use crate::helpers::{element_key, find_pane, render_terminal_profile_icon};
use crate::notifications::{ActivitySection, activity_badge, activity_section};
use crate::view_models::TooltipView;
use crate::{HhApp, THEME, pane_status_color};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    ParentElement, StatefulInteractiveElement, Styled, div, linear_color_stop, linear_gradient, px,
    rgb, rgba,
};
use hh_protocol::{BotThread, Tab};

/// Height of one thread row; the list shows at most `THREAD_ROWS_VISIBLE`.
const THREAD_ROW_HEIGHT: f32 = 22.0;
const THREAD_ROWS_VISIBLE: usize = 15;
/// Height of `THREAD_ROWS_VISIBLE` thread rows.
const THREAD_LIST_MAX_HEIGHT: f32 = THREAD_ROW_HEIGHT * 15.0;

impl HhApp {
    pub(crate) fn render_sidebar_bots(&self, cx: &mut Context<Self>) -> AnyElement {
        let workers = self
            .session
            .snapshot
            .as_ref()
            .map(|snapshot| {
                bot_workers(snapshot, |pane_id| {
                    self.session
                        .pane_states
                        .get(&pane_id)
                        .is_some_and(|state| state.exited)
                })
            })
            .unwrap_or_default();
        let rows = self
            .bots_workspace()
            .map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .filter(|tab| tab.bot.is_some())
                    .map(|tab| {
                        let owned = workers.get(&tab.id).map_or(&[][..], Vec::as_slice);
                        self.render_bot_row(tab, owned, cx)
                    })
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

    fn render_bot_row(
        &self,
        tab: &Tab,
        workers: &[BotWorker<'_>],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let agent = tab.bot.as_ref().map(|bot| bot.agent).unwrap_or_default();
        let pane = bot_pane(tab);
        let selected = self.bot_is_selected(tab);
        let exited = pane.is_some_and(|pane| self.pane_exited(pane.id));
        let status = pane.map(|pane| pane.status).unwrap_or_default();
        let dot = if exited {
            THEME.dim
        } else {
            pane_status_color(status).unwrap_or(THEME.border_strong)
        };
        let status_label = activity_badge(status, exited);
        let subtitle = match workers.len() {
            0 => agent.display_name().to_owned(),
            1 => format!("{} · 1 worker", agent.display_name()),
            count => format!("{} · {count} workers", agent.display_name()),
        };
        let threaded = has_threads(tab);
        let expanded = self.bot_threads_expanded(tab);
        let threads = if expanded {
            self.render_bot_threads(
                tab,
                if selected {
                    THEME.accent_soft
                } else {
                    THEME.sidebar
                },
                cx,
            )
        } else {
            None
        };
        let chips = workers
            .iter()
            .map(|worker| self.render_worker_chip(worker, cx))
            .collect::<Vec<_>>();
        div()
            .id(("bot-row", element_key(tab_id)))
            .mx(px(6.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(5.0))
            .cursor_pointer()
            .flex()
            .flex_col()
            .gap(px(5.0))
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
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
                                    .child(subtitle),
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
                    .child(Self::bot_card_button(
                        ("new-bot-thread", element_key(tab_id)),
                        "＋",
                        "New thread",
                        cx.listener(move |this, _, _, cx| {
                            this.open_bot_thread(tab_id, None, cx);
                            cx.stop_propagation();
                        }),
                    ))
                    .when(threaded, |element| {
                        element.child(Self::bot_card_button(
                            ("bot-threads-toggle", element_key(tab_id)),
                            if expanded { "⌄" } else { "›" },
                            if expanded {
                                "Hide threads"
                            } else {
                                "Show threads"
                            },
                            cx.listener(move |this, _, _, cx| {
                                this.toggle_bot_threads(tab_id, cx);
                                cx.stop_propagation();
                            }),
                        ))
                    }),
            )
            .children(threads)
            .when(!chips.is_empty(), |element| {
                element.child(
                    div()
                        .pl(px(28.0))
                        .flex()
                        .flex_wrap()
                        .gap(px(4.0))
                        .children(chips),
                )
            })
            .into_any_element()
    }

    fn pane_exited(&self, pane_id: uuid::Uuid) -> bool {
        self.session
            .pane_states
            .get(&pane_id)
            .is_some_and(|state| state.exited)
    }

    /// A small square icon button on a bot card.
    fn bot_card_button(
        id: impl Into<gpui::ElementId>,
        glyph: &'static str,
        tooltip: &'static str,
        on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        div()
            .id(id)
            .flex_none()
            .w(px(18.0))
            .h(px(18.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .font_family(".SystemUIFont")
            .text_xs()
            .text_color(rgb(THEME.muted))
            .hover(|element| {
                element
                    .bg(rgb(THEME.border))
                    .text_color(rgb(THEME.foreground))
            })
            .on_click(on_click)
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.to_owned(),
                })
                .into()
            })
            .child(glyph)
            .into_any_element()
    }

    /// The bot's thread rows in a list capped at `THREAD_ROWS_VISIBLE` rows,
    /// fading at the bottom when it scrolls. `bg` is the card background.
    fn render_bot_threads(&self, tab: &Tab, bg: u32, cx: &mut Context<Self>) -> Option<AnyElement> {
        let threads = self.bot_threads.lists.get(&tab.id)?;
        if threads.is_empty() {
            return None;
        }
        let current = bot_pane(tab).map(|pane| pane.id);
        let now = now_ms();
        let rows = threads
            .iter()
            .enumerate()
            .map(|(index, thread)| self.render_bot_thread_row(tab, thread, index, current, now, cx))
            .collect::<Vec<_>>();
        let overflows = threads.len() > THREAD_ROWS_VISIBLE;
        Some(
            div()
                .relative()
                .pl(px(28.0))
                .child(
                    div()
                        .id("bot-threads")
                        .max_h(px(THREAD_LIST_MAX_HEIGHT))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
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
                                linear_color_stop(rgba(bg << 8), 0.0),
                                linear_color_stop(rgb(bg), 1.0),
                            )),
                    )
                })
                .into_any_element(),
        )
    }

    /// One thread: live dot, title, pin, and age. Clicking shows it in the
    /// bot's terminal; right-click pins or unpins it.
    fn render_bot_thread_row(
        &self,
        tab: &Tab,
        thread: &BotThread,
        index: usize,
        current: Option<uuid::Uuid>,
        now: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let is_current = thread.pane_id.is_some() && thread.pane_id == current;
        let live = thread
            .pane_id
            .and_then(|pane_id| find_pane(&tab.layout, pane_id))
            .map(|pane| {
                if self.pane_exited(pane.id) {
                    THEME.dim
                } else {
                    pane_status_color(pane.status).unwrap_or(THEME.border_strong)
                }
            });
        let title = thread
            .title
            .clone()
            .unwrap_or_else(|| "New thread".to_owned());
        let open_id = thread.id.clone();
        let menu_thread = thread.clone();
        div()
            .id(("bot-thread", index))
            .h(px(THREAD_ROW_HEIGHT))
            .flex_none()
            .px(px(6.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .font_family(".SystemUIFont")
            .text_xs()
            .when(is_current, |element| element.bg(rgb(THEME.elevated)))
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_bot_thread(tab_id, Some(open_id.clone()), cx);
                cx.stop_propagation();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.open_bot_thread_menu(tab_id, &menu_thread, event.position, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded_full()
                    .when_some(live, |element, color| element.bg(rgb(color))),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .flex_1()
                    .truncate()
                    .text_color(rgb(if is_current {
                        THEME.foreground
                    } else {
                        THEME.muted
                    }))
                    .child(title),
            )
            .when(thread.pinned, |element| {
                element.child(div().flex_none().text_color(rgb(THEME.dim)).child("📌"))
            })
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(THEME.dim))
                    .child(relative_time(now, thread.updated_ms)),
            )
            .into_any_element()
    }

    /// One worker tab: its status dot and name. Clicking opens that tab.
    fn render_worker_chip(&self, worker: &BotWorker<'_>, cx: &mut Context<Self>) -> AnyElement {
        let (workspace_id, tab_id, pane_id) = (worker.workspace.id, worker.tab.id, worker.pane_id);
        let title = worker
            .tab
            .custom_title
            .as_deref()
            .unwrap_or(&worker.tab.title)
            .to_owned();
        let dot = if worker.exited {
            THEME.dim
        } else {
            pane_status_color(worker.status).unwrap_or(THEME.border_strong)
        };
        let needs_you =
            activity_section(worker.status, worker.exited) == Some(ActivitySection::NeedsYou);
        let on_screen = self
            .layout
            .focused_pane
            .is_some_and(|pane_id| find_pane(&worker.tab.layout, pane_id).is_some());
        let border = if on_screen {
            THEME.accent
        } else if needs_you {
            dot
        } else {
            THEME.border
        };
        let tooltip = format!(
            "{title} — {} · {}",
            activity_badge(worker.status, worker.exited),
            worker.workspace.title
        );
        div()
            .id(("bot-worker", element_key(tab_id)))
            .max_w(px(140.0))
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(rgb(border))
            .when(on_screen, |element| element.bg(rgb(THEME.accent_soft)))
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .hover(|element| element.bg(rgb(THEME.elevated)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_bot_worker(workspace_id, tab_id, pane_id, cx);
                cx.stop_propagation();
            }))
            .tooltip(move |_, cx| {
                cx.new(|_| TooltipView {
                    text: tooltip.clone(),
                })
                .into()
            })
            .child(
                div()
                    .flex_none()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded_full()
                    .bg(rgb(dot)),
            )
            .child(
                div()
                    .min_w(px(0.0))
                    .truncate()
                    .font_family(".SystemUIFont")
                    .text_xs()
                    .text_color(rgb(if worker.exited {
                        THEME.dim
                    } else {
                        THEME.muted
                    }))
                    .child(title),
            )
            .into_any_element()
    }
}
